# RFC: Deliver a reset to the caller as an event

- Status: draft

## Summary

Deliver a terminal reset (RIS, `ESC c`) to the caller as `ChildEvent::Reset` on
the existing child-event channel, and stop clearing the queue when a reset
happens. A reset is a command the child sends to the terminal, not a value the
terminal drew, and once the crate lets the caller hold child-derived state (the
clipboard today, the title next) the caller needs to be told when that state is
invalidated. Today the only reset the crate implements wipes its own fields and
silently discards any request the caller had not drained; this RFC makes the
reset itself the last thing the caller sees, so nothing is dropped and the
caller can invalidate whatever it was holding.

This RFC covers RIS only. The other reset, DECSTR (`CSI ! p`), is deliberately
left out: the crate does not implement it at all yet, and what it should and
should not reset is not yet settled here.

Because only RIS is delivered, and DECSTR is not, the event is named `Reset`
and not `HardReset`. "Hard" only means anything against "soft"; with no
`SoftReset` beside it, the qualifier reserves a distinction that is not yet
made. If DECSTR is later delivered, that is the RFC that adds a second variant
and, if a contrast is then wanted, renames this one.

This RFC also renames the internal `soft_reset` method to `reset_child_state`.
The old name borrows "soft reset" from DECSTR, but the method is not DECSTR
handling: it is the crate's cleanup when the child session ends or is replaced,
and it touches state DECSTR does not. The new name says what the method does -
it resets the state the crate derived from the child - and stops colliding with
the terminal's own term.

## Motivation

A host application that embeds a child session keeps some of the child's state
on the child's behalf: it mirrors a clipboard selection, and once the title
proposal lands it will hold a window title. That state has a lifetime the host
did not choose. A child can reset the terminal, and when it does, the host's
copy is stale - the title it is showing, the selection it mirrored - and the
host has no way to learn that.

There is a narrower problem already in the code. `reset_child_state` clears the
queue of not-yet-taken requests, which was the right thing when the queue held only a
clipboard write with no owner: a reset meant the session was over, and acting on
an ask from before it would be wrong. But "discard silently" is exactly the
failure this series exists to remove. A child that writes a clipboard selection
and then, in the same `feed`, sends RIS has its clipboard write vanish: the
caller never sees it, and never sees that it was cancelled. The queue is a
delivery point, and a reset should be delivered through it, not used to empty
it.

Once the reset is an event, the discard becomes unnecessary and the two
purposes separate cleanly. Resetting the crate's *own* fields stays what it is
today - internal work on internal state. Telling the *caller* that their
derived state is stale is one new event, appended after anything already
queued. A caller that drains the queue in order sees the pre-reset requests
first, then the reset, and knows that everything before the reset is now moot.
No request is lost, and the ordering rule (arrival order) is the only rule.

## Guide-level explanation

Before, a caller had no reliable way to notice a reset. It could watch for
observable side effects - the title going empty, cells blanking - but those are
consequences, not the event, and a reset that happens to leave the same visible
state is invisible. Any clipboard write queued just before the reset was
discarded before the caller could read it.

After, the reset is what the caller sees last in the drain:

```rust
while let Some(event) = term.take_child_event() {
    match event {
        ChildEvent::SetClipboard { selection, text, append } => {
            host_clipboard.set(selection, &text, append);
        }
        ChildEvent::Reset => {
            // Everything the host derived from the child is now stale.
            host_clipboard.clear();
            host_title.reset_to_default();
        }
    }
}
```

A child that sets a selection and then resets produces two events, in that
order: the write, then `Reset`. The write is no longer swallowed. A caller
that only wants the end state drains the queue and keeps the last thing that
matters to it.

## Reference

```rust
pub enum ChildEvent {
    SetClipboard {
        text: Vec<u8>,
        selection: ClipboardSelection,
        append: bool,
    },
    Reset,
}
```

`Reset` is a unit variant: RIS carries no payload and names no resource, so
there is nothing to put in it. The kind of reset is spelled in the variant name
rather than in a `style` field, so a second kind (a future `SoftReset`) would be
a second variant that a caller ignores by simply not matching it.

What `reset_child_state` does changes in one line. It keeps resetting the state
the crate owns - cells, cursor, pen, modes, scroll region, scrollback, the title
field, the pending-test flags - exactly as today, and it replaces

```rust
self.osc_requests.clear();
```

with

```rust
self.child_events.push_back(ChildEvent::Reset);
```

so the reset is appended to whatever is already queued rather than discarding
it.

This RFC also renames the method itself, `soft_reset` to `reset_child_state`,
for the reason in the summary. It is an internal method; the rename is not part
of the event channel and carries no compatibility cost beyond callers of the
crate's own test and setup paths.

RIS is a visible change - the screen is cleared - so it already moves
`revision()` through the normal visible-state comparison; the reset event adds
nothing to that and does not itself move `revision()` (no child event does).

## Alternatives considered

- **A boolean or a counter instead of an event.** A caller could poll "has a
  reset happened since I last looked". Rejected: it is a second notification
  mechanism beside the channel, has to be cleared by the caller with the same
  take/drain shape the channel already provides, and gives no ordering relative
  to the requests around it, which is the point of putting it on the queue.
- **Name the variant `HardReset` and reserve `SoftReset` now.** The idea is
  that adding DECSTR later is then only an addition, and the pair reads as a
  pair. Rejected: with no `SoftReset` in the enum, "hard" is a qualifier against
  nothing, and it fights the crate's own `soft_reset` method (which this RFC
  renames) that is not DECSTR handling. The kind of reset is better spelled only
  when there is more than one; a later DECSTR proposal adds the second variant
  and renames this one if a contrast is then useful.
- **`Reset { style: ResetStyle }` instead of a variant per kind.** One variant
  that carries the kind. Rejected in favor of the variant-per-kind shape: it
  keeps a caller that handles one kind from writing an inner match, and it reads
  the same way the other variants do (each names what happened). The cost is
  that a new reset kind is a new variant and therefore a breaking change for an
  exhaustive match, which is true of every variant added to this enum anyway.
- **Keep clearing the queue, and additionally push the reset.** The reset would
  arrive, but requests queued before it would still be dropped without the
  caller seeing them, which is the current bug plus a new event. Rejected.
- **Push the reset before clearing.** Ordering aside, it still requires a clear,
  and the clear is the part that loses data. Rejected with the above.
- **Do nothing; leave reset as an invisible side effect on crate state.** The
  caller has to infer it from consequences, which is unreliable, and the
  clipboard-write-then-reset case keeps losing the write. Rejected.

## Unresolved questions

- **Which field name is public enough to need the rename?** This RFC assumes
  the channel is called `child_events` and the accessor `take_child_event()`,
  which is the subject of the rename proposal in this series. If that proposal
  is settled first, this one follows it; if not, this one is written against
  the current `osc_requests` / `take_osc_request()` names and updated when the
  rename lands.
- **Does `Reset` want to be emitted for the alternate screen too?** RIS resets
  both screens in the crate. There is nothing for this variant to say about
  which one, since the event is "the child reset the terminal", not "a screen
  changed"; open whether a caller ever needs the distinction, which this RFC
  assumes it does not.
- **What is the reset's relationship to the reply buffer?** A reset clears the
  crate's pending reply bytes today only because the buffer is empty when it
  matters. Whether a reset should also drop a pending reply (a reply owed for a
  query the child sent before the reset) is not settled here; this RFC leaves
  the reply path untouched.
- **Should the title field be reset at all once the title is an event?** Today
  `reset_child_state` clears the `title` field. If the title proposal moves the
  title onto the channel, there is no field left to clear, and the reset event
  is what tells the caller to drop its own. The two proposals meet here; this
  RFC is written against the current field and defers the interaction.
- **Is `reset_child_state` the right new name?** It avoids the DECSTR
  collision, but the method also runs on paths that are not "the child exited".
  Whether it should instead be named for what it resets, or for the trigger, is
  not settled; this RFC only fixes the one name that is actively misleading.

## Future possibilities

- **DECSTR (`CSI ! p`) as `SoftReset`.** The crate does not implement DECSTR
  yet, and what a *soft* reset should reset (modes and SGR, or more; whether a
  title survives it) is not agreed here. When it is, it is one more unit variant
  and one more arm in the tokenizer, with no change to this RFC's shape. If it
  is only internal state, it may need no variant at all; if it is delivered, a
  `SoftReset` beside `Reset` is the natural next step, and the pair would then
  justify a `Reset`-to-`HardReset` rename here.
- **More reset-scoped events.** If a caller ever needs to distinguish "the
  screen was cleared" from "the terminal was reset", that is a separate event,
  not a field on this one, for the same reason the kind is spelled in the
  variant name.
- **A crate-owned definition of "stale".** Today the caller decides what to
  invalidate when it sees `Reset`. If the crate later holds more derived state,
  it might offer to enumerate what a reset invalidates; that would be a new
  method, not a change to this event.
