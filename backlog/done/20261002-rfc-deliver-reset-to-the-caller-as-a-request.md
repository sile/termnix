# RFC: Deliver a reset to the caller as a request

- Status: rejected

## Summary

Deliver a terminal reset (RIS, `ESC c`) to the caller as `ChildRequest::Reset`
on the existing child-request channel, and stop clearing the queue when a reset
happens. A reset is a command the child sends to the terminal, not a value the
terminal drew: it is a fact about the child's behaviour, and the crate is a
general-purpose terminal emulator that does not know what the host does with
that fact. The crate therefore reports it the same way it reports every other
child-to-host message, and lets the host decide what, if anything, to do. Today
the only reset the crate implements wipes its own fields and silently discards
any request the caller had not drained; this RFC makes the reset itself the last
thing the caller sees, so nothing is dropped.

This RFC covers RIS only. The other reset, DECSTR (`CSI ! p`), is deliberately
left out: the crate does not implement it at all yet, and what it should and
should not reset is not yet settled here.

Because only RIS is delivered, and DECSTR is not, the request is named `Reset`
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

The crate is a general-purpose terminal emulator. It does not assume a
particular host, and it cannot know which of the child's actions a host cares
about, or what state a host derives from the child on the child's behalf. A
reset should be reported for that reason alone, not because some specific host
happens to mirror a clipboard or hold a title; those are examples, not the
argument. The argument is that the crate's contract is to surface the child's
behaviour on a single channel, and a reset is part of that behaviour.

Four properties of the channel make the reset belong on it:

- **Observability.** A child can reset the terminal. That is something the
  child did, and a terminal emulator should let the host observe it directly
  rather than infer it from side effects. The reset that happens to leave the
  same visible state is invisible to inference and visible here.
- **Symmetry.** Child-to-host information already travels on one channel: a
  clipboard write is a request, and the title proposal puts the title there
  too. A reset is the same kind of message - a command the child sent to the
  terminal - and leaving it as an internal side effect would be the one hole in
  an otherwise uniform contract.
- **Not dropping data.** `reset_child_state` clears the queue of not-yet-taken
  requests. That was defensible when the queue held only a clipboard write with
  no owner, but it means a child that writes a selection and then, in the same
  `feed`, sends RIS has its write vanish: the caller never sees it, and never
  sees that it was cancelled. "Discard silently" is exactly the failure this
  series exists to remove, and the queue is a delivery point - a reset should be
  delivered through it, not used to empty it.
- **No coercion.** Delivering the reset does not tell the host what to do. A
  host with no use for it ignores the variant; a host that keeps child-derived
  state uses it to invalidate that state. The crate reports, the host decides.

There is a tension with YAGNI worth naming. RIS is not common: well-behaved
full-screen programs usually reset modes and SGR rather than issuing RIS, and
the programs that do send RIS are the recovery paths - `reset(1)`, `tput
reset`, a child re-initialising after it damaged the terminal. A `ChildRequest`
variant is a cost for every host that matches exhaustively. The resolution is
that the cost is not specific to the reset: it is the general cost of adding
any variant to the channel, and the channel's whole purpose is to carry these
messages. The right question is therefore not "how often does RIS happen" but
"is a reset the child's behaviour that the channel exists to report". The four
properties above are the case that it is.

Once the reset is a request, the discard becomes unnecessary and the two
purposes separate cleanly. Resetting the crate's *own* fields stays what it is
today - internal work on internal state. Reporting the reset to the *caller*
is one new request, appended after anything already queued. A caller that
drains the queue in order sees the pre-reset requests first, then the reset,
and knows that everything before the reset is now moot. No request is lost, and
the ordering rule (arrival order) is the only rule.

## Guide-level explanation

Before, a caller had no reliable way to notice a reset. It could watch for
observable side effects - the title going empty, cells blanking - but those are
consequences, not the reset itself, and a reset that happens to leave the same
state is invisible. Any clipboard write queued just before the reset was
discarded before the caller could read it.

After, the reset is one more thing the caller sees in the drain. What a host
does with it is up to the host; the example below is one shape, not the
contract. A host that holds nothing derived from the child simply does not
match the variant:

```rust
while let Some(request) = term.take_child_request() {
    match request {
        ChildRequest::SetClipboard { selection, text, append } => {
            host_clipboard.set(selection, &text, append);
        }
        ChildRequest::Reset => {
            // One possible host: everything it derived from the child is now
            // stale. A host with no such state just ignores this arm.
            host_clipboard.clear();
            host_title.reset_to_default();
        }
    }
}
```

A child that sets a selection and then resets produces two requests, in that
order: the write, then `Reset`. The write is no longer swallowed. A caller
that only wants the end state drains the queue and keeps the last thing that
matters to it.

## Reference

```rust
pub enum ChildRequest {
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
self.child_requests.clear();
```

with

```rust
self.child_requests.push_back(ChildRequest::Reset);
```

so the reset is appended to whatever is already queued rather than discarding
it.

This RFC also renames the method itself, `soft_reset` to `reset_child_state`,
for the reason in the summary. It is an internal method; the rename is not part
of the request channel and carries no compatibility cost beyond callers of the
crate's own test and setup paths.

RIS is a visible change - the screen is cleared - so it already moves
`revision()` through the normal visible-state comparison; the reset request
adds nothing to that and does not itself move `revision()` (no child request
does).

## Alternatives considered

- **A boolean or a counter instead of a request.** A caller could poll "has a
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
  caller seeing them, which is the current bug plus a new request. Rejected.
- **Push the reset before clearing.** Ordering aside, it still requires a clear,
  and the clear is the part that loses data. Rejected with the above.
- **Do nothing; leave reset as an invisible side effect on crate state.** The
  caller has to infer it from consequences, which is not something a
  general-purpose library can offer: inference depends on which side effects a
  particular host watches, and a reset that leaves the same state is invisible
  to any of them. Concrete hosts would each build their own detection, which is
  the per-host arrangement the single channel exists to avoid. The
  clipboard-write-then-reset case also keeps losing the write. Rejected.

## Unresolved questions

- **Which field name is public enough to need the rename?** Settled: the
  rename in this series has landed, so the channel is `child_requests` and the
  accessor `take_child_request()`. This RFC is written against those names;
- **Does `Reset` want to be emitted for the alternate screen too?** RIS resets
  both screens in the crate. There is nothing for this variant to say about
  which one, since the request is "the child reset the terminal", not "a screen
  changed"; open whether a caller ever needs the distinction, which this RFC
  assumes it does not.
- **What is the reset's relationship to the reply buffer?** A reset clears the
  crate's pending reply bytes today only because the buffer is empty when it
  matters. Whether a reset should also drop a pending reply (a reply owed for a
  query the child sent before the reset) is not settled here; this RFC leaves
  the reply path untouched.
- **Should the title field be reset at all once the title is a request?** Today
  `reset_child_state` clears the `title` field. If the title proposal moves the
  title onto the channel, there is no field left to clear, and the reset request
  is what tells the caller to drop its own. The two proposals meet here; this
  RFC is written against the current field and defers the interaction.
- **How much does the crate commit to being host-agnostic?** This RFC argues
  from the crate being a general-purpose library and keeps host-specific state
  (clipboard, title) as examples only. That is a stance, not yet a documented
  contract. Whether the crate's docs should state that child-to-host messages
  travel on one channel and that the crate does not assume what a host does
  with them is not settled here; if they should, that is documentation work
  beside this request, not a change to its shape.
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
- **More reset-scoped requests.** If a caller ever needs to distinguish "the
  screen was cleared" from "the terminal was reset", that is a separate request,
  not a field on this one, for the same reason the kind is spelled in the
  variant name.
- **A crate-owned definition of "stale".** Today the caller decides what to
  invalidate when it sees `Reset`. If the crate later holds more derived state,
  it might offer to enumerate what a reset invalidates; that would be a new
  method, not a change to this request.

## Outcome

Superseded by the unified event channel before it was implemented. The crate
now reports a reset as `Event::TerminalReset`, a value on the single event
stream the caller drains, rather than as a `ChildRequest` variant; a reset is
state the terminal reached, and the event stream is where that is reported.
The two proposals also disagree in the other direction: this one argued that a
reset must not clear not-yet-taken requests, but the landed behavior does clear
them, on the grounds that a pending request belongs to the session being reset.
The rename it proposed (`soft_reset` to `reset_child_state`) was carried out
separately, for the reason given above.
