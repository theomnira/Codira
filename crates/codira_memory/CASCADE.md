<!--
Copyright (c) 2026 Omnira CJSC
Author: Tunjay Akbarli
Date: November 6, 2026

Functionality: Design of Cascade, the experimental reclamation engine of the
Codira memory system (deferred ownership accounting, concurrent cycle
reclamation and safe-reclamation epochs).
-->

# Cascade: deferred ownership accounting and cycle reclamation

Status: **experimental, off by default.** Nothing in this document changes
how compiled Codira programs manage memory today. Programs run on the
tracing collector (`src/gc/mark_sweep.rs`), and Cascade only runs when a
host enables it on a heap through `MarkSweep::enable_cascade`. The compiler
does not emit retains or releases yet. Section 9 lists what that integration
needs.

The code is in `src/gc/cascade/`. Tests are in `tests/cascade.rs`,
the model-checking tests are in `src/gc/cascade/loom_tests.rs`, and the
benchmark is in `benches/cascade.rs`.

## 1. Goal and the invariant

Cascade reclaims memory *before* a tracing collection would:

- A **unique** object is reclaimed at its last ownership point.
- A **shared** object is reclaimed when its count of owning references
  drops to zero. The count is maintained by optimised reference counting,
  never by an atomic increment and decrement on every assignment.
- **Thread-local** references cost a plain load and store, or nothing at
  all.
- **Cycles** are found by a candidate-based cycle detector that does a
  bounded amount of work per step.

One invariant overrides everything else:

> **An object is never reclaimed while any thread can still legally access
> it.**

Codira has no destructors, and the decision to add them is open, so
reclamation is unobservable to a program. A reclaimed cell is only memory
that the allocator can reuse. Cascade never runs user code. The only effect
of reclaiming earlier is that the heap stays smaller.

Wherever the runtime cannot establish that an object is dead, it keeps the
object and leaves it to the tracing collector. A count that saturates, an
object rooted by the host, a cycle pass that exceeds its budget, a mutator
that stops taking part in epochs: each of these makes Cascade do *less*,
never something unsafe.

## 2. Where the metadata lives

Every object already carries a 32-byte header (`gc::heap::ObjectHeader`).
Generated code relies on its first word, and the size-class table is built
around its size. The header does not grow. The 8-byte `inline_capacity`
field became two 4-byte fields:

| field             | meaning                                                                 |
|-------------------|-------------------------------------------------------------------------|
| `inline_extent`   | small cell: payload bytes available inline; large object: offset of the payload from the header (the capacity is recomputed from the allocation layout) |
| `ownership`       | the 32-bit Cascade ownership word, 0 (`UNTRACKED`) unless Cascade manages the object |

Every object is still exactly one cell, and a cell is 32 bytes of header
plus its payload, as before. When Cascade is off, the only cost is that
allocation stores a zero into `ownership`. That store lands in the cache
line the allocator is already writing, and Section 11 measures its cost.

A side table was the alternative. `benches/cascade.rs` measures one keyed by
header address (an `FxHashMap` behind a mutex, the cheapest correct shared
map available without new dependencies), so that its cost can be compared
with the in-header word.

### Ownership word layout

```
bit  0..3   state     UNTRACKED | UNIQUE | BORROWED | SHARED | RETIRING | RECLAIMED
bit  3      PUBLISHED reachable by more than one thread; never cleared
bit  4      ZCT       queued in a zero-count table or retire list
bit  5      BUFFERED  queued as a cycle candidate
bit  6      CYCLE     member of a cycle set awaiting validation
bit  7      TOUCHED   counted by an increment since its cycle set was computed
bit  8..11  borrows   active scoped borrows (0..=7)
bit 11..32  count     owning heap references (0..=2^21-1); at the maximum it is
                      sticky and only the tracing collector reclaims the object
```

**Who may write the word.** The writer depends on publication, and this
rule is what lets the counting avoid atomic read-modify-write operations:

- **Unpublished** object: only the thread that allocated it, its *owner*,
  reads or writes the word, with plain (relaxed) loads and stores. No other
  thread can hold a reference to it (Section 4).
- **Published** object: only the Cascade collector writes the word, while it
  holds the heap lock. Mutators only read `PUBLISHED`, which never changes
  once set. They never touch the count; they log their changes instead
  (Section 3).
- **Tracing resynchronisation** writes every word while the world is stopped
  (Section 8).

## 3. The ownership state machine

```
            alloc
              │
              ▼
          ┌────────┐ borrow     ┌──────────┐
          │ UNIQUE │──────────▶│ BORROWED │
          │        │◀──────────│          │
          └────────┘ end_borrow └──────────┘
   second     │  │                   │ second reference, publish
   reference, │  │ release_unique    ▼
   publish    │  │ (validated) ┌──────────┐
              └──┼────────────▶│  SHARED  │◀──────┐ resurrect (count > 0 when
                 │             └──────────┘       │ re-examined: only for a
                 │                   │ count = 0  │ conservative false positive)
                 │                   ▼            │
                 │             ┌──────────┐───────┘
                 │             │ RETIRING │  zero-count table / retire list
                 │             └──────────┘
                 │                   │ validated: no heap refs, no local refs,
                 ▼                   ▼            no host roots, not in a cycle set
          ┌──────────────────────────────┐
          │ RECLAIMED (cell freed; reset │
          │ to UNTRACKED on reuse)       │
          └──────────────────────────────┘
```

**Invariants.** These are checked by `tests/cascade.rs` and by debug
assertions.

1. *Count soundness.* For every tracked object, the applied count is at
   least the number of heap fields that hold the object at the last
   completed epoch boundary (Section 5), counting the delayed root
   increments. The count never under-approximates.
2. *Publication closure.* A published object references only published
   objects. An unpublished object is referenced only from its owner's
   locals and from unpublished objects of the same owner.
3. *Single writer.* An unpublished object's word is written only by its
   owner, and a published object's word only by the collector (or by the
   resynchronisation while the world is stopped).
4. *Reclaim condition.* An object is reclaimed only if all of the following
   hold:
   - its count is zero and its host root count is zero;
   - no thread holds it locally;
   - it is not borrowed and not a member of a pending cycle set;
   - either it is unpublished and its owner is the thread reclaiming it, or
     it is published and the epoch boundary that observed the zero has
     completed.

   A cycle is reclaimed only after the validation of Section 6.
5. *Sticky saturation.* A count that reached its maximum is never changed
   again by counting, so it cannot wrap around.

**Unique.** `alloc` returns a `UNIQUE` object that its single local
reference owns. When the compiler can prove that a program point is the
object's last use (this needs the uniqueness analysis, Section 9), it emits
`release_unique`. The runtime does **not** trust that proof blindly. It
reclaims immediately only if all of these hold:

- the state is `UNIQUE`;
- the object is not published;
- the count is 0;
- there are no borrows;
- the thread holds exactly one local reference.

If any check fails, the release drops one local reference, and the object
follows the shared path below. A wrong proof therefore costs a deferred
reclamation, never a premature one. This is the "a failed optimisation never
weakens safety" rule made concrete.

**Borrowed.** A scoped, non-owning reference to a unique object (`borrow` /
`end_borrow`). An object with an active borrow is never reclaimed. A
`release_unique` during a borrow is refused and falls back as above. In
this branch, borrows stay on the owner's thread. Lending an object to
another thread is a publication.

**Shared.** The object has, or had, more than one reference. An unpublished
shared object is counted directly by its owner, with a plain load and a
plain store. A published one is counted by the deferred scheme of Section 5.

**Retiring.** The count reached zero and the object sits in a zero-count
table. An unpublished object sits in its owner's table. A published one sits
in the collector's retire list. Re-examining it either reclaims it or, if
the count is positive again, returns it to `SHARED`.

## 4. Local references and the publication transition

**Local references** are references held in a thread's locals: stack slots
and registers in compiled code. They are *never counted*. This is deferred
reference counting in the sense of Deutsch and Bobrow (CACM 1976): only heap
references are counted, and the locals are examined when a zero count is
about to be acted on.

Cascade has two sources of local references:

- **Compiled code (future):** the conservative scan of the thread's stack
  that the safepoint protocol already provides (`gc::safepoint`,
  `gc::stack_scan`). A word that happens to look like a reference only keeps
  an object alive. Objects never move, so conservativeness is safe.
- **The Rust API (today):** `Mutator::hold` and `Mutator::drop_local`
  maintain a thread-local, non-atomic multiset of locally held objects. It
  stands in for stack slots until compiler integration, and it is how the
  tests drive the mechanisms. It is also what the tracing collector uses as
  extra roots for API threads, because their references live in Rust data
  structures that the conservative scan does not see.

**Publication** is the transition from "only my thread can reach this" to
"any thread may". `Mutator::publish(o)` walks the unpublished subgraph
reachable from `o`. Every object in it is unpublished and owned by the
current thread (invariant 2). The walk sets `PUBLISHED` on each object with a
release store. From then on, the object's count belongs to the collector.
The count transfers as it is, because the owner applied its changes
immediately. Objects with a zero count go into the thread's next packet as
*checks*, so the collector examines them even if no count change ever
mentions them.

Publication must happen-before the operation that makes the object
reachable by another thread. `Mutator::store` into a published parent
publishes the child first and then stores the field with release ordering.
A future channel send, task spawn or global store must do the same.
Publication is one-way. An object is never unpublished, which would require
knowing that every other thread has dropped it.

### Relation to the String buffer owner word

String buffers (`codira_runtime`'s `string` module) carry an 8-byte *owner
word*: the thread pointer of the thread that allocated the buffer. That
thread alone may raise the buffer's fill in place, with plain loads and
stores. Any other thread appending copies. The two mechanisms answer
different questions:

- The owner word decides **who may write** the bytes above a buffer's fill.
- Cascade's ownership word decides **who counts** references to an object
  and **when** the object may be reclaimed.

They agree where they overlap. A `Mutator` is bound to one OS thread (it is
not `Send`), so an object a mutator allocates is unpublished and owned by
the same OS thread whose pointer a String buffer allocated there would
carry. Publication changes neither the owner word nor who may append in
place. It only moves the counting to the collector. The owner word was
written before the buffer became reachable and never changes, so it stays
valid for the buffer's whole life. Neither mechanism weakens the other:
`UNIQUE` grants no right to append, and the owner word grants no right to
reclaim.

Reclamation interacts with the owner word in one place: cell reuse. A cell
that Cascade reclaims is reused only through the allocator, and the
allocator writes a fresh owner word before the new buffer is reachable.
This is the same order that the owner word's argument relies on after a
tracing sweep.

String buffers are not allocated through Cascade on this branch. Making
them Cascade-managed is part of compiler integration (Section 9):
appending in place writes bytes, never references, so it needs no write
barrier.

## 5. Deferred counting and the epoch scheme

For published objects, Cascade uses the epoch-based concurrent reference
counting of Bacon, Attanasio, Lee, Rajan and Smith, "Java without the
Coffee Breaks: A Nonintrusive Multiprocessor Garbage Collector" (PLDI 2001),
the *Recycler*: mutators log, the collector applies at epoch boundaries.
The logs are not coalesced. Coalescing them in the manner of Levanoni and
Petrank, "An On-the-fly Reference Counting Garbage Collector for Java"
(OOPSLA 2001), is listed under future work in Section 10.

**Mutator side.**

- A store into a field logs `inc(new)` and `dec(old)` into the thread's
  local buffers, two plain `Vec` pushes. It performs no atomic operation and
  takes no lock.
- When both objects are unpublished, the owner applies the changes directly
  instead.
- At a *handshake* (`Mutator::safepoint`, or automatically when a buffer
  fills), the thread:
  1. reclaims its own unpublished zero-count objects;
  2. takes a *root snapshot*: the published objects it holds locally;
  3. moves its buffers, the snapshot and the publication checks into a
     packet, and pushes the packet onto the collector's inbox (one mutex
     acquisition per handshake);
  4. announces the current global epoch.

**Epochs.** The global epoch `E` advances from `b` to `b+1` only after every
registered mutator has announced `b`, or is quiescent. Before advancing, the
collector processes *boundary b*:

1. apply the increments of segment `b-1` (the operations logged before each
   thread's handshake into `b`), and the root snapshot of `b` as increments;
2. then apply the decrements of segment `b-2`, and the root snapshot of
   `b-1` as decrements.

All increments come before all decrements. Decrements are delayed by one
epoch because thread `U` may already be in segment `b` when it creates a
reference that thread `T`, still in segment `b-1`, then destroys. `T`'s
decrement must not be applied before `U`'s increment. This is the Recycler's
argument. Its consequence is invariant 1, and with it the reclaim rule:

> A published object whose count is zero after boundary `b` is referenced by
> no heap field created up to segment `b-1` and not yet destroyed, and by no
> thread's locals at that thread's handshake into `b`. Any later reference
> would have to be copied from one of those. So the object is garbage, and
> garbage stays garbage.

**Increments and decrements are kept separately; they are never netted.**
A thread's segment may contain an `inc(o)` that pairs with its own `dec(o)`.
Netting them would apply that decrement one epoch early, which breaks the
cross-thread argument above (a decrement in `T`'s segment can pair with an
increment that `U` logs in a later segment). The buffers are therefore two
logs. The collector, as the single writer of published counts, applies them
with plain stores. That, rather than coalescing, is where the saving over
per-assignment atomics comes from.

**Retire list.** Objects found at zero after boundary `b` enter the retire
list (`RETIRING`). They are reclaimed in later steps, a bounded number per
step, once the boundary that observed the zero has completed. Reclaiming an
object logs nothing. It decrements its published children directly (the
collector is their counts' single writer), and a child that drops to zero is
reclaimed in the same pass. The child is garbage by the same argument: its
only counted references came from a garbage parent, and the root snapshot
of the same boundary has been applied.

**Quiescence.** A thread that will not touch published objects for a while
(for example, while it blocks) calls `Mutator::quiescent(f)`. The thread
hands over its root snapshot, which the collector re-applies at every
boundary while the thread stays quiescent, so the epoch can advance without
it. A thread that neither handshakes nor declares itself quiescent stops
the epoch. Nothing unsafe follows: Cascade simply reclaims nothing new, and
the tracing collector (Section 8) takes over under memory pressure.

**Relation to classic epoch-based reclamation.** Fraser's EBR (Practical
lock-freedom, 2004) frees a retired node after every thread has passed two
epoch announcements. Here the handshake into `b` is the announcement, and
the root snapshot replaces the critical-section discipline: a thread may
hold a published object across epochs, as long as the object appears in
its snapshots. `src/gc/cascade/epoch.rs` holds the announcement and advance
protocol, isolated so that it can be model-checked
(`src/gc/cascade/loom_tests.rs`).

## 6. Cycle reclamation

Counting never reclaims a cycle. Cascade uses trial deletion, as introduced
by Martínez, Wachenchauzer and Lins (1990) and made linear by Bacon and
Rajan, "Concurrent Cycle Collection in Reference Counted Systems"
(ECOOP 2001). It uses their candidate-buffer formulation, and the Σ- and
Δ-tests of the concurrent variant validate results computed while mutators
run.

**Candidates.** When a decrement leaves a count *non-zero*, the object may
be the last external entry into a garbage cycle. It is marked `BUFFERED`
and queued, at most once. Three other events count as such a decrement,
because each removes a reference that the pass would treat as external:
- dropping the last local reference to a counted, unpublished object;
- a queued zero count found positive again when it is re-examined (it was
  stored again while queued, possibly into a cycle through itself);
- the collector's check of a publication root. Published candidates go to the collector's
candidate queue, and unpublished ones to the owner's local queue. A
candidate is the root of a possible garbage cycle, not a member of one.
Objects that are never decremented to a non-zero value never become
candidates. That filter, from Bacon and Rajan's paper, removes most of the
work.

**Synchronous pass (unpublished candidates, owner thread).** At a
handshake, the owner runs the three phases over its unpublished objects
only:

- *MarkGray*: compute trial counts with internal edges subtracted, where a
  local hold or a host root counts as an external reference.
- *Scan*: mark gray objects with a positive trial count, and everything
  they reach, black again.
- *CollectWhite*: what remains white is garbage.

Published children are outside the scope of the pass: by publication
closure they cannot be part of a cycle with unpublished objects. Nothing
else mutates the objects while the owner runs the pass, so the result is
exact. It reclaims at once: the owner decrements unpublished children
directly and logs the decrements of published children.

**Concurrent pass (published candidates, collector).** Mutators keep
running and mutating fields while the collector computes, so the result is
validated before it is acted on:

1. *Compute* (in one step, bounded by the step's budget). The collector runs
   MarkGray, Scan and CollectWhite on *trial counts kept in a side map*. The
   real counts are not modified, and only published, allocated objects are
   traversed. The white set `G` becomes a pending cycle set. Its members get
   `CYCLE` (the retire list defers them) and lose `TOUCHED`.
2. *Wait* for at least one full epoch boundary to complete. While applying
   increments (operations and root snapshots), the collector sets `TOUCHED`
   on every object it increments.
3. *Validate* (the Δ-test and the Σ-test). Reject `G` if:
   - any member is `TOUCHED`, whether by an increment or by a root snapshot
     (some thread held it at its handshake); or
   - any member's count reached zero (the retire list will handle it); or
   - for some member, its count minus the edges from other members (read
     now) is non-zero.

   Otherwise `G` had no external heap references and was in no thread's
   locals at the cut of the last boundary. By invariant 1 and the Recycler
   argument, `G` is garbage.
4. *Reclaim* every member, and decrement the published children that are
   outside `G`. When a set is rejected, its members lose `CYCLE`. One
   live member rejects the whole set, but the set can also contain
   independent garbage cycles that no later decrement would ever queue
   again. So members that passed both tests on their own become candidates
   again, and the next pass separates them: a live one turns black. A member
   whose count reached zero while its set was pending is flagged `ZCT`, but
   is not queued for retirement until its set is released. Validation never
   accepts it, so it is queued exactly once.

**Why validation is sound when fields change during the pass.** Suppose `G`
is in fact reachable at the cut. Then either:

- some thread held a member at its handshake, so the member is `TOUCHED`;
  or
- some field outside `G` holds a member `m`. That reference was created in
  a segment whose increment has been applied, and its decrement, if any, has
  not yet been applied. So `count(m)` exceeds the number of edges from inside
  `G`, the Σ-test fails at `m`, and rejecting at any member rejects the set.

  An edge that a mutator adds *after* the cut only makes the internal
  count of its target larger. To add it, the mutator needs the edge's source
  in hand, and following how it got that source leads back to an external
  reference or a handshake root, which the test catches.

**Bounds and the pressure valve.**

- Every step does at most `budget` units of work. A unit is an applied
  count change, a reclaimed object or a traversed edge.
- Each pass is seeded with a batch of candidates. The batch halves whenever
  a pass overflows its budget and doubles again when one fits (up to 4096).
  The overflowing pass's roots go back on the queue. Only a single candidate
  whose graph exceeds `CascadeConfig::max_cycle_pass` work units is
  abandoned. It loses `BUFFERED`, is counted in
  `CascadeStats::abandoned_passes`, and is left to tracing.
- The candidate queue is a hash set, and growing it rehashes every entry.
  `CascadeConfig::max_candidates` (65,536 by default) caps it, so that
  rehashing never costs a step more than a bounded amount. A candidate that
  does not fit is counted in `CascadeStats::candidate_overflows`. It puts
  the following steps under pressure, which ends in a tracing collection.
- The bytes of retiring and candidate objects are reported as
  `deferred_bytes` (`Cascade::stats`, and `MarkSweep::cascade_deferred_bytes`
  for heap accounting).
- When `deferred_bytes` exceeds `CascadeConfig::pressure_bytes`, the heap
  is within 1/8 of its limit, or a candidate overflowed since the last
  tracing collection, the step budget is multiplied by
  `pressure_multiplier`. If the pressure persists for
  `pressure_steps_before_trace` steps, Cascade runs the tracing collector
  (`MarkSweep::collect_blocking`, after the step has released its locks), so
  deferred garbage is never deferred forever.

## 7. Step latency and work accounting

`Cascade::step(budget)` (or `MarkSweep::cascade_step`) is the only place
collector work happens. It holds the heap's write lock for its whole
duration, which also serialises it against the tracing collector (Section
8). Within one step, it works through these phases in order, and stops when
the budget runs out:

1. continue the current boundary: apply up to `budget` increments, then
   decrements;
2. when the boundary completes, examine the checks and the zero-count
   objects, advance the epoch, and validate a pending cycle set;
3. reclaim up to the remaining budget from the retire list;
4. run one trial-deletion batch on the remaining budget.

Starting a boundary moves whole logs and shares each root snapshot
between its increments and its later decrements. It never copies entries,
so its cost is proportional to the number of packets, not to the number of
logged operations. A boundary in progress keeps the epoch from advancing, so the increments
and decrements of one boundary are never interleaved with another's, even
across steps. A step completes at most two boundaries, because an idle
boundary costs no work units. Children that a cycle reclamation releases
are retired in the same step. Section 11 reports the measured step latency
at several budgets.

## 8. The tracing collector as the backstop

The tracing collector is unchanged when Cascade is off. When Cascade is on,
the collector calls the Cascade hook twice per collection, while the world
is stopped and the heap's write lock is held:

1. **Before marking:** every object held locally by an API mutator is an
   extra root.
2. **After sweeping:** *resynchronisation*. The tracing collector has just
   proved exactly which objects are live, and the heap fields are the truth.
   The hook therefore:
   - recomputes every tracked count from scratch: zero, then one increment
     per reference field of every live tracked object;
   - drops every logged operation and every pending delayed decrement, all
     of which describe changes already present in the heap;
   - drops the pending root decrements (their increments are gone).

   The recount applies every decrement at once, including the ones that
   Section 5 deliberately delays. That delay covered a thread which loaded
   an object after its last snapshot, and the cover is now gone. The
   resynchronisation restores it with its own root snapshot of every
   thread's exact local references, available because the world is
   stopped. The snapshot is applied immediately, and its decrements wait
   one boundary longer than a normal snapshot's: until after the first
   boundary whose snapshots were all taken after this point.

   It then rebuilds the queues from survivors only. The sweep may have
   freed what they name, so only addresses are compared, and nothing is
   dereferenced:
   - root snapshots that were handed over but not yet applied are kept,
     filtered to survivors;
   - standing snapshots are filtered too;
   - the owners' zero-count and candidate queues keep their surviving
     entries;
   - every surviving published object that some dropped piece of work
     named (a decrement, a check, a retire or candidate entry, a pending
     cycle set), and every published survivor with a zero count, becomes a
     check for the next boundary.

   Without the last step, garbage that the conservative stack scan
   retained, or a cycle whose candidate-making decrement was dropped, would
   fall out of every queue.

So the tracing collector reclaims every garbage cycle, including cycles
that an abandoned pass or an obstinate mutator left behind. It also
repairs every count that drifted upwards: references inside garbage that
was freed, saturated counts, and hot-reload migrations that dropped
fields. `MarkSweep::map_memory` runs the same resynchronisation after a
migration.

The lock order is fixed: stop-the-world, then the heap lock, then
Cascade's collector state; mutator handshakes take only the inbox lock.
Cascade steps never poll a safepoint while holding a lock, so a
stop-the-world can always make progress.

## 9. What compiler integration needs (not on this branch)

This branch adds no retain or release insertion, and compiled programs keep
exactly today's behaviour. Integration needs the following, most of it from
the escape and uniqueness analysis:

1. **Allocation:** allocate through the Cascade entry point when the
   program opts in, so the word starts as `UNIQUE`.
2. **Field stores:** a write barrier at every store of a reference into a
   heap object, array elements included. The barrier loads the old value,
   stores the new one, and logs inc and dec. It is cheap when both objects
   are unpublished (plain load and store) and two buffer pushes otherwise.
   The Rust API swaps the field of a *published* object atomically. Two
   threads may race on it there, and each overwritten reference must be
   released exactly once. This swap is most of the measured cost of a
   published store (Section 11). Compiled code could use a plain load and
   store instead, if the language rules out unsynchronised races on
   reference fields (Section 10).
   The barrier must cover runtime-internal stores too: array `push` and
   `grow`, `Map` and `String` internals, hot-reload migration.
3. **No local counting.** Locals are found by stack scanning, so the
   compiler emits nothing for copying a reference between locals or
   passing it as an argument. This is where deferred RC wins.
4. **`release_unique`** at the last use of an object that the analysis
   proves unique and non-escaping, and **`borrow` / `end_borrow`** around
   scoped borrows. A wrong proof only defers (Section 3).
5. **Publication** before every operation that lets another thread reach an
   object: channel send, task spawn, captures moved into a spawned task,
   stores into globals or into published objects. The analysis can elide it
   where it proves the object is already published, or never escapes.
6. **Handshakes** at safepoints. The thread scans its *own* stack for the
   root snapshot. `stack_scan` already captures the callee-saved registers,
   and it would be extended from a test-only function. An inline poll keeps
   the cost at one relaxed load when no epoch is pending.
7. **Precise stack maps** (`aurora/stackmaps`) would make the snapshots
   exact, but they are not required. Conservative snapshots are safe.

The tests use a Rust-level `Mutator` API that has exactly these entry
points.

## 10. Assumptions and what is unverified

- **Data-race freedom of compiled code.** The single-writer argument
  assumes that two threads never race on a reference field without
  synchronisation. The Rust API uses atomic field accesses, so it is sound
  in Rust terms. In compiled code, publication happens-before sharing only
  if the language rules out data races on references, or if every racy
  store goes through the barrier with release ordering. *Unverified*: this
  is a language-level decision.
- **The concurrent pass reads fields while mutators write them.** The Rust
  API uses atomic loads. Generated code would use plain stores, and reading
  those is formally a race (word-sized and aligned in practice). Precise
  stack maps do not change this.
- **The soundness arguments are informal.** They follow the published
  Recycler and Bacon–Rajan proofs, adapted to separate inc and dec logs,
  root snapshots, publication and resynchronisation. The adaptations were
  checked by randomized testing against a reachability oracle, and the
  epoch announcement and publication protocols were model-checked with
  `loom` at small bounds (preemption bound 3). Two mutations of the
  protocol were checked to make the models fail: announcing before pushing
  the packet, and storing the field before publishing the word. `loom`'s
  model of relaxed atomics is incomplete, and it did *not* catch
  weakening the announcement store from release to relaxed. The models
  therefore establish the interleaving properties, not the weakest
  orderings that would suffice. There is no mechanised proof of the whole
  scheme.
- **Testing found five bugs, which are fixed.** The randomized oracle and
  the concurrent stress test found, in order:
  1. a resynchronisation dropped the delayed-decrement cover (use after
     reclaim);
  2. a resynchronisation re-queued snapshots that named freed objects
     (double free);
  3. a rejected cycle set leaked the garbage cycles it shared with live
     objects;
  4. a cycle-set member that reached zero was queued twice (double free);
  5. a queued zero count that rose again lost its candidacy (leak).

  Each test now passes repeatedly, but the coverage is that of randomized
  testing.
- **Tests share the safepoint registry.** The conservative scan sees every
  parked thread in the process, including threads of unrelated tests. The
  Cascade tests therefore live in their own test binary
  (`tests/cascade.rs`), so their many threads do not add false retentions
  to the heap tests' exact accounting.
- **Liveness.** Reclamation of published objects depends on every mutator
  handshaking or being quiescent. One stuck thread stops RC reclamation, and
  only the pressure valve then reclaims, through tracing.
- **Not measured on other platforms.** Everything was run on x86-64 Linux
  only (Section 11). `loom` does not model weak memory beyond C11, and
  nothing was run on AArch64.
- **Compaction is off while Cascade is attached.** The experimental
  compaction (`CODIRA_GC_COMPACT=1`, `MarkSweep::set_compaction`) evacuates
  sparse blocks and rewrites heap slots. Cascade's thread-local logs, root
  snapshots, standing roots, retire list, candidate queue, pending cycle
  set and local holds all store raw header addresses, and evacuation would
  not update them. The collector therefore compacts only when no Cascade
  engine is attached (`compaction && cascade.is_none()` in `collect_impl`).
  Making the two work together needs one of two designs:
  - the resynchronisation already runs inside the stop and visits every
    surviving tracked object, so it could rewrite those addresses through
    the forwarding table, which every queue would have to be reachable from
    (today the packets in the inbox and the thread-local structures are);
  - or Cascade could pin every block that holds an object it names.

  Neither is built.
- **Not built:** user-visible destructors (deliberately), cross-thread
  borrows, unpublication, compiled-code self stack scans for snapshots,
  coalescing of the logs (Levanoni–Petrank style: one entry per object per
  segment), generational candidate filtering, parallel collector threads.

## 11. Measurements

**Machine and method.** One cloud VM: Intel Xeon @ 2.10 GHz, 4 vCPUs,
15 GB of RAM, Linux 6.18, stable Rust 1.97.1, release profile. The VM is
shared, so the noise is real. Every number below is the median of 11
samples (31 for the header comparison), with the minimum in parentheses.
They come from `cargo bench -p codira_memory --bench cascade` and
`--bench gc`, run on the branch after its rebase onto main. Heaps in the
Cascade benchmark have pacing turned off; otherwise paced collections
would reclaim the garbage before Cascade does.

### Header change (Cascade off)

The ownership word replaces the upper half of `inline_capacity`.
`--bench gc` was run alternately on origin/main and on this branch, two
rounds each. Medians are in ns per object, collection in ms:

| workload | main, round 1 | main, round 2 | branch, round 1 | branch, round 2 |
|---|---|---|---|---|
| alloc 200,000 x 8 B, fresh heap | 23.64 | 24.65 | 23.75 | 23.81 |
| alloc 200,000 x 8 B, after a collection | 23.64 | 22.63 | 22.81 | 22.95 |
| collect 100,000 live + 100,000 dead | 2.09 | 2.10 | 2.14 | 2.13 |

There is no difference beyond run-to-run noise. The collection is 1 to 2
percent slower on the branch in both rounds, which is within that noise
but stated here anyway. These rounds were run before the rebase, on the
previous main.

### Cost per assignment (one increment plus one decrement), ns

| what | median (min) |
|---|---|
| plain field store, no barrier | 0.65 (0.63) |
| atomic swap of a field (the published barrier's store) | 8.65 (7.91) |
| naive atomic RC, 1 thread | 15.61 (15.15) |
| naive atomic RC, 4 threads on one object | 62.48 (57.59) |
| side-table RC (`Mutex<FxHashMap>`), 1 thread | 25.70 (24.82) |
| **Cascade, unpublished** (owner counts with plain stores) | **10.61 (10.39)** |
| **Cascade, published, mutator side** (swap + two log pushes) | **28.05 (26.54)** |
| Cascade collector, per applied inc or dec | 4.17 (3.69) |
| Cascade, published, 4 threads + collector thread (wall) | 11.46 (9.53) |
| `hold` + `drop_local` (API stand-in for stack slots; compiled code pays nothing) | 5.92 (5.57) |
| `alloc` + `release_unique` | 72.77 (60.61) |
| `MarkSweep` alloc, tracing every 64K objects | 30.55 (28.08) |

**Reading these honestly:**

- **Unpublished counting wins** against single-thread atomic RC: 10.6 ns
  against 15.6 ns. Most of the 10.6 ns is the barrier's checks, not the
  count.
- **The published mutator path loses** against single-thread atomic RC:
  28 ns against 15.6 ns. Most of the difference is the field *swap*, which
  is needed only because the Rust API allows racing stores (Section 9).
  The collector then spends about 4 ns more per applied change.
- **Under contention Cascade wins clearly.** With 4 threads, Cascade's wall
  time is 11.5 ns per assignment, against 62 ns for naive atomic RC on one
  shared object. These are different workloads, though: here each thread
  stores into its own published object, and the counts are applied by a
  separate collector thread.
- **The side table loses** to the in-header word: 26 ns against a few ns of
  plain loads and stores.
- **`release_unique` is not yet cheap.** An allocation plus a release costs
  73 ns, against 31 ns for an allocation that is left to tracing. The
  release takes the heap's write lock once per object. Freeing into the
  thread's own allocation run, without the lock, is future work.

### Cycle reclamation throughput (M objects per second)

200,000 garbage objects, in rings of the given size, from the drop to
complete reclamation:

| rings of | published, concurrent pass | steps (budget 65,536) | unpublished, owner's pass | one tracing collection |
|---|---|---|---|---|
| 2 | 2.09 (1.82) | 52 | 2.93 (2.64) | 120 (111) |
| 16 | 1.84 (1.54) | 37 | 4.12 (3.49) | 121 (94) |
| 128 | 3.05 (2.56) | 26 | 4.57 (3.08) | 149 (58) |

**Tracing is 30 to 60 times faster at reclaiming pure garbage.** It marks
nothing here (no live objects) and sweeps by blocks. Trial deletion pays
for hash-map trial counts, and for validating each set one boundary later.
Cascade's case for cycles is therefore not throughput. It is that the work
is incremental and bounded per step (the next table), and that it needs no
stop-the-world over the whole heap. Comparing against a tracing collection
over a large *live* heap was not measured.

### Step latency (3 mutator threads, collector on the remaining core)

The mutators produce published garbage rings and published stores without
pause. Times are those the step held the heap lock; the wait for the lock
is reported separately:

| budget | steps | median | p99 | p99.9 | max | lock wait, max |
|---|---|---|---|---|---|---|
| 256 | 46,001 | 16.1 us | 63.7 us | 129 us | 3.73 ms | 3.22 ms |
| 1,024 | 11,839 | 58.7 us | 165 us | 308 us | 4.05 ms | 1.90 ms |
| 4,096 | 3,377 | 260 us | 579 us | 885 us | 1.27 ms | 1.57 ms |
| 16,384 | 2,448 | 241 us | 725 us | 1.26 ms | 2.84 ms | 1.30 ms |

At 16,384 the median work per step is 7,302 units: the queues ran dry
before the budget did.

**These are measured bounds on one machine, not guarantees:**

- The p99.9 scales with the budget, at about 0.3 to 0.5 us per unit at
  small budgets.
- The maxima, a few ms, are outliers of a few steps in tens of thousands.
  The two causes measured are rehashing the candidate queue, which the cap
  bounds (Section 6), and preemption of the collector thread on the shared
  VM.
- Before the cap, a single step reached 43 ms of CPU time while it rehashed
  1.8 million queued candidates. That is why the cap exists.
- Thread CPU time can exceed the lock-held time. It includes the wait for
  the lock and the pressure-triggered tracing collections, neither of which
  is counted as the step's own work.
