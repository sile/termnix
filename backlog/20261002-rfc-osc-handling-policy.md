# RFC: A policy for OSC handling (umbrella)

- Status: draft

## Summary

Define how `TerminalState` should treat an OSC sequence, as a policy over
identifier space rather than one decision per sequence. Today the crate
recognizes exactly two OSC numbers and silently discards every other one, so
"what termnix does with an OSC" is really "whatever the last RFC that added a
number happened to decide". This RFC does not add a sequence. It states a
*rule* - what makes an OSC something termnix must hold as state, something it
emits as an event, and something it does neither to and should pass through -
and it splits the work of applying that rule into separate RFCs, one per
decision that can land on its own.

This is an **umbrella**: it fixes the policy and the boundaries between the
follow-on proposals. Each follow-on is an ordinary RFC that can be accepted or
rejected on its own, and this one is not settled until they are.

## Motivation

The current handling of OSC is a list, not a rule. `osc_dispatch`
([`src/terminal_emu.rs`](../src/terminal_emu.rs)) matches `"0" | "2"` and stores
a window title, matches `"52"` and records a clipboard request, and ignores
every other identifier. The rustdoc on `TerminalState`
([`src/terminal.rs`](../src/terminal.rs)) restates the same thing as a feature
list - "OSC 0/2: window title (stored); OSC 52: clipboard request" - and the
first line of `osc_dispatch` gives the only reason the rest are dropped:
their payloads "are ignored so their payloads never appear as printable text".

That reason is about one failure mode, not about policy. It says what must not
happen to an unrecognized OSC (its bytes must not leak into the grid as text);
it does not say what *should* happen to it. The current answer - make it
vanish - is the one option that loses information for every caller. A host
application that embeds a child session (the code that drives a PTY and renders
the child's output through `TerminalState`) sits between two parties that both
speak OSC: the child that writes sequences, and the host terminal that would
answer to them. termnix is the tokenizer in the middle, and for any number it
does not model it is currently a one-way valve that drops what it cannot
understand.

There is also an asymmetry that a list cannot explain and a policy can. OSC 0
and 2 (the title) are held as *state*: `title()` is a non-destructive getter and
the value is a plain property of the terminal. OSC 52 is held as a *consumed
once event*: `take_clipboard()` is destructive, and the value is gone after it
is read. Nothing in the current docs says why one is state and the other an
event, or which rule the next sequence should follow. Without that rule, every
new OSC is an argument from scratch, and the argument has been decided
differently each time.

The cost is concrete at the boundary the crate straddles. A host application
that wants to support an OSC termnix does not model - a working directory
announcement, a prompt mark, a terminal-specific extension - has no way to see
it: the bytes have already been consumed and dropped by the time the host has a
`TerminalState` to ask. To support such a sequence the host would have to
re-tokenize the PTY stream itself beside `vte`, duplicating the framing whose
rules it is trying to centralize. The one thing the crate is best positioned to
provide - a tokenizer that hands sequences to a caller instead of discarding
them - is the one thing it does not do for numbers it does not know.

Nothing here proposes to make termnix *interpret* more OSC numbers. Most of
them should stay opaque to the crate. The problem is narrower: an unmodelled
sequence is discarded rather than offered, and there is no stated rule for when
a sequence must be held, offered, or dropped.

### What an OSC actually is

A policy needs a correct picture of the thing being sorted. OSC is not a
notification channel; it is an escape-sequence frame,
`ESC ] <number> ; <arguments...> ST` (where `ST` is `BEL` or `ESC \`), that
namespaces
text-like traffic to the terminal by number. Its properties, and why the list
in the rustdoc could not have become a rule by accumulation, are:

- **Numbers are independent and open.** Each number is its own mini-protocol
  with its own argument grammar, and vendors claim new numbers freely
  (xterm, iTerm2's 1337, kitty's 99, and so on). There is no registry that
  makes a number "the" meaning, only convention. A crate cannot enumerate the
  space and should not pretend to.
- **Most numbers are setters, not notifications.** The category named
  "notification" is the exception: OSC 0/2 set a title, OSC 4 and 10-12 set
  colors, OSC 52 *sets* a selection, OSC 7 sets a working directory, OSC 8
  sets a hyperlink attribute. The name is "Operating System Command" because
  the original use was telling the terminal to do something, and that is still
  the dominant direction.
- **Direction is carried by the arguments, not the number.** The same number
  is a set and a query depending on whether an argument is the literal `?`:
  `OSC 4 ; 1 ; ?` asks for a palette entry where `OSC 4 ; 1 ; color` sets it,
  and the reply reuses the same number. So a number is not "incoming" or
  "outgoing"; one number can be both, and a crate that models one direction and
  drops the other has modelled a number halfway.
- **The payload is opaque.** termnix already treats it that way for OSC 52
  (the payload and the unmodelled selection name are `Vec<u8>`, not `String`),
  and the framing rule is the same for every number: `vte` splits on `;`, which
  is a property of the frame, not of any one sequence's grammar.

## Guide-level explanation

The proposal is a rule, so it is stated as one. Sort an OSC by two questions:

1. **Does termnix interpret it?** That is, does the crate decode the arguments
   into fields of its own (`ClipboardRequest`, a title string, a color)?
2. **Does the crate use it internally?** That is, does the effect belong to the
   visible grid or to the machinery that produces it - the cells, the pen, the
   modes, the scroll region - so that a later paint depends on it?

The answer picks the home:

| interprets? | uses internally? | home |
| ----------- | ---------------- | ---- |
| yes | yes | **state**: a getter, like `title()` is today |
| yes | no | **event**: a take, like `take_clipboard()` is today |
| no | no | **passthrough**: offered to the caller unparsed |
| no | yes | impossible; using a value internally requires decoding it |

The rule is worth stating because it resolves the asymmetry the current docs
give no account of. A title is *interpreted* (termnix keeps the string) but not
*used* (nothing the terminal draws depends on it), so the rule puts the title
in the **event** row. This is not a statement that the title must move - that is
a separate proposal, below - only that the rule has an answer for it, and the
answer is not "state" by any property other than history.

It also gives the passthrough row its first member. A sequence termnix does not
interpret and does not use is one the crate has no opinion about; the useful
thing to do with it is hand it to the caller that does, with the framing already
done and the payload untouched. That is a capability the crate does not have
today: there is no way for a caller to receive an OSC that is not 0, 2, or 52.

From a caller's point of view the change is that "unknown OSC" stops meaning
"lost" and starts meaning "delivered". A caller that wants a working-directory
announcement, or a prompt mark, or a vendor extension, registers nothing and
models nothing: it reads the offered sequence and decides for itself. A caller
that wants none of them ignores what it is offered, exactly as it ignores the
`None` from `take_clipboard()` today. Nothing a caller does today changes
meaning: the title stays where it is until the title proposal moves it, OSC 52
keeps its current accessor until the event proposal replaces it, and sequences
that were dropped are dropped until the passthrough proposal lands.

## Reference-level explanation

### The policy

An OSC identifier is handled by exactly one of three mechanisms, in this order
of precedence:

1. **State**, when termnix decodes the arguments and the decoded value is part
   of the terminal's own condition - the grid, the pen, the modes, the scroll
   region, or anything else a later paint reads. Such a value is a property of
   the terminal, read with a non-destructive getter, and participates in
   visible-change detection (`revision()`). A value in this row may also have an
event, but only if the change itself is a thing a caller must act on; the
   property and its notification are then separate.
2. **Event**, when termnix decodes the arguments but the value is not part of
   the terminal's condition - the child is asking the caller to do something or
   telling it something, and termnix holds it only to hand it over. Such a
value is consumed once, taken rather than read, and is deliberately not part of
   `revision()` because it draws nothing.
3. **Passthrough**, when termnix does not decode the arguments at all. The
   sequence is offered to the caller as the identifier plus the raw arguments
   `vte` produced, with no interpretation. A passthrough value is not terminal
   state, is not an event until a caller makes it one, and draws nothing.

A sequence termnix does not recognize must not be silently discarded as the
*only* behavior. "Drop it" is a defensible choice for a caller that asked for
nothing, but it cannot be the crate's whole answer to an identifier, because it
is the choice that withholds the information from every caller. If a number is
not worth interpreting, the fallback is passthrough, not a hole.

### Applying the policy

Sorting the sequences this crate already touches, and the neighbors a
follow-on is likely to be about. The two questions are asked *about a sequence
termnix would have to model*, not about what the crate does today: a number
that is unmodelled now is placed by what the rule says it must become if it is
modelled at all. "Today" is a separate column so the two are not confused.

| identifier | today | if modelled: interprets? | if modelled: uses internally? | home |
| ---------- | ----- | ------------------------ | ----------------------------- | ---- |
| OSC 0, 2 (title) | state | yes | no | event (the title proposal decides the move) |
| OSC 52 (clipboard) | event | yes | no | event (already a take) |
| OSC 7 (working directory) | dropped | no | no | passthrough |
| OSC 8 (hyperlink) | dropped | yes | yes | state, once modelled |
| OSC 4, 10, 11, 12 (colors) | dropped | yes | yes | state, once modelled |
| OSC 133 (prompt marks) | dropped | no | no | passthrough |
| every other number | dropped | no | no | passthrough |

The `interprets?` column for OSC 8 and the color sequences is `yes` on purpose:
they reach the state row only by being decoded into a hyperlink attribute or a
palette entry, which is the one combination the four-row rule names as
impossible when it is not done. Before that decoding they are passthrough like
anything else; the table states where they belong, not what happens first.

Two rows carry the weight of the policy:

- **OSC 7 and 133 are passthrough, not events.** They are told *about* the
  child, not requested *of* the caller: a working directory and a prompt mark
  are facts the caller may want, with no action owed. That is exactly the
  passthrough row, and it is most of the reason the row needs to exist at all.
- **OSC 8 and the color sequences are state, not passthrough.** A hyperlink
  attribute and a palette entry are not messages; they change what the grid
  means when it is drawn, so a later paint depends on them. Handing them to a
  caller as bytes would leave the crate painting cells with a stale pen and a
  missing attribute. They are out of scope here (see below) but included in the
  table because the rule has to place them, and it places them on the state
  side.

### The split

This policy is applied by separate RFCs. Each is a normal proposal that can be
accepted or rejected on its own; none of them is implied to be accepted by this
one. The dependency order is that the event mechanism is a container the others
put things into, so it comes first; passthrough answers where an unmodelled or
`?` sequence goes, so the question of read requests lands there.

- **The event mechanism.** Introduce a single consumed-once event channel on
  `TerminalState` (`take_event()`), and decide whether the existing clipboard
take folds into it or sits beside it. The RFC that adds it owns the shape of
the `Event` type; this umbrella fixes only that the rule above assigns each
  sequence a home, and that a family of consumed-once events is what the rule's
  "event" row produces.
- **Passthrough.** Expose unmodelled OSC sequences to the caller, with the
  framing already done. Open questions to settle there, not here: whether the
  offered value carries the identifier and split arguments as `vte` produced
  them or the raw sequence bytes; whether passthrough is a take (a caller drains
  it) or a queue (a caller keeps all of them); and, if a queue, its bound and
  what happens on overflow.
- **The title.** Decide whether the window title moves from state to the event
  row, as the rule says it may. The proposal owns the question of whether the
  move is worth it, including the loss of the current-value getter (a caller
  that wants the latest title would then keep it itself) weighed against the
  removal of the title's special-case bookkeeping (`title_changed` and its
  exclusion from the `Copy`-field compare).
- **Grid-affecting sequences.** Modelling OSC 4 and 10-12 as pen/palette
  state, and OSC 8 as a hyperlink attribute, is left to its own proposals. This umbrella deliberately does not design them: the work is a
  grid/pen change, not an OSC-policy change, and it is larger than the rest of
  the row. The policy's only claim about them is the table's: they are state.
- **Read requests (`?`).** A query asks the terminal to answer on the same
  number, so the reply is a byte to write *back* to the child. That is the shape
  of the existing reply buffer
  ([`pending_reply_bytes()`](../src/terminal.rs) and
  [`advance_reply_bytes()`](../src/terminal.rs)), not of a consumed-once event,
  and it is also what passthrough delivers (a caller that owns the answer can
  answer from the raw sequence). Which of the two owns `?` is settled in the
  passthrough proposal, not here.

### What does not change

- **The grid must not be reachable by passthrough.** Offering an unmodelled
  sequence must never let its bytes become cells, which is the property the
  current `_ => {}` arm protects. Passthrough is a separate channel, not a
  change to how printable text is recognized.
- **The reply buffer is not an event.** It is a byte stream a caller may write
  partially, with an explicit consumed-length; the existing RFC that chose that
  shape over a single-variant action enum stands, and nothing here reopens it.
- **The `&self` accessors on `TerminalState` stay `&self`.** A value in the
  state row is a property read non-destructively. Only the event row is a take.

## Drawbacks

- **A rule constrains the next decision, including a decision that might have
  been better made ad hoc.** Once "passthrough, not a hole" is the policy, a
  future number cannot be silently dropped just because dropping is convenient;
  the proposer has to place it in the table. That is the cost of not having the
  list grow by accident, and it is the point.
- **Passthrough is new API surface whose usage is entirely caller-defined.**
  termnix commits to delivering sequences whose meaning it does not know, so the
  crate cannot help a caller get the meaning right, and the choice of what to
  do with an `OSC 7` or an `OSC 133` becomes the caller's problem in a way it
  was not when the bytes were dropped. The crate trades "no caller can use
  these" for "every caller must decide about these".
- **An umbrella is process, not code.** It adds a document whose payoff is
decisions in other documents. If the follow-ons are never written, it is a
  policy with no implementation, which is worse than the list it replaced only
  if the list was genuinely fine - and the argument here is that it was not,
  because it could not explain its own title/clipboard asymmetry.
- **The state row may grow for reasons unrelated to OSC.** If a follow-on models
  OSC 4/10-12 or OSC 8, termnix gains color and hyperlink state that this crate
  has so far declined to model. That cost belongs to those proposals, but the
  policy is what makes them expressible, so it shares the blame for the
  direction.

## Rationale and alternatives

- **Keep the list; add numbers one at a time.** This is today, and it produced
  the asymmetry in the first place: the title is state and the clipboard is an
  event for no stated reason. A list also has no answer for a number nobody has
  written an RFC for, which is the common case. Rejected because the missing
  thing is the rule, not another row.
- **Make termnix interpret every OSC, so nothing needs passthrough.** This
  makes the crate the owner of every vendor extension and every future number,
  which is unbounded work in a crate whose boundary is "parse PTY output", not
  "implement the terminal". It also contradicts the existing choice to hold the
  OSC 52 payload as opaque bytes: the crate does not need to know what a
  sequence means to carry it. Rejected.
- **Keep dropping unmodelled sequences, but document why.** This is honest and
  cheap, and it is what the current `osc_dispatch` comment already does. It
  still withholds the sequence from every caller, and it leaves a host that
  embeds a child unable to support an extension without a second tokenizer
  beside `vte`. Rejected: the failure is the withholding, not the lack of a
  comment, and a documented drop is still a drop.
- **Make every unmodelled sequence a consumed-once event (no separate
  passthrough row).** Simpler table, one mechanism. But a working directory and
  a prompt mark are not "taken once and acted on"; a caller wants the latest,
  or all of them, and a take that overwrites loses one of the two shapes. It
  also forces a meaning onto sequences the crate does not know - promoting them
  to "events" asserts they are requests, which OSC 7 and 133 are not. Rejected:
  passthrough is the honest shape for "the crate has no opinion".
- **Answer every `?` from state.** For a number termnix models as state (a
  palette entry, a default color), answering a query is well-defined. For a
  number it does not model (OSC 52 with nothing to report, an unmodelled
  extension), there is nothing true to answer, and the crate's existing
  principle is that a probe gets no reply rather than a wrong one
  (see the DA2/DA3 handling in
  [`src/terminal_emu.rs`](../src/terminal_emu.rs)). So `?` is not "answer it"
  as a policy; it is "route it" - to a state reply when the crate holds the
  value, to passthrough when it does not. That routing is the passthrough
  proposal's job.
- **Do nothing.** The cost is that the list in the rustdoc keeps standing in
  for a rule, every new OSC re-litigates the same question, and a host
  embedding a child cannot support an unmodelled extension without its own
  tokenizer. The asymmetry between `title()` and `take_clipboard()` stays
  unexplained, which means the next sequence is as likely to be placed by
  accident as by argument.

## Unresolved questions

- **Is "drop" ever allowed as a third fallback?** The policy says passthrough
  is the fallback for an unmodelled number, which leaves no room for deliberate
  dropping. A number whose payload is known to be dangerous to deliver (if any
  such turns out to exist) might warrant dropping instead, and whether the
  policy needs an explicit "do not deliver" row is open.
- **Does a state-row value ever need its own event?** The policy allows a
  state value to have a separate notification when the *change* is something a
  caller must act on, but no current or planned sequence is an example, so
  whether that clause has content is open. If it does not, the policy can drop
  it; if it does, the event mechanism proposal is where it lands.
- **Does the title move at all?** The rule places the title in the event row,
  but "the rule says it may move" is not "it should move". The title proposal
  owns this, including whether the `title_changed` bookkeeping is worth
  removing.
- **Where does `?` land?** Routed to the reply buffer for modeled state, to
  passthrough otherwise - but whether the passthrough proposal is the right
  owner of that routing, or whether it deserves its own proposal, is open.

## Future possibilities

- If the passthrough row lands, the crate becomes a general OSC demultiplexer:
  it frames sequences and routes each to state, event, or caller, and a host
  embedding a child can support any sequence the host understands without a
  second tokenizer. The policy is the part of that which has to be right first,
  because it is what the routing encodes.
- A host-terminal abstraction (a type that owns the outer terminal and knows
  how to answer it) would turn passthrough from "hand the caller bytes" into
  "the crate handles the sequences an outer terminal can answer", which is a
  different boundary than this RFC draws but not a contradictory one: the
  policy's passthrough row is "the crate has no opinion", and a host
  abstraction is one way for a *caller* to supply an opinion.
- The state row is where the crate grows toward modelling more of the terminal
  (colors, hyperlinks, and whatever a prompt-mark protocol needs). The policy
  gives each such proposal a place to argue from, which is the most this
  umbrella can offer them.
