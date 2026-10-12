# `epp` on the scheduled (`run`) route — localization findings

Status: **NOT fixed in `beam_rt`.** The defect is a **collection-site gap in a sibling crate**: the
receiver's mature cell *is* covered by the major collection's from-set and *is* reachable from the
receiver's own **published** roots, yet a **delivery-driven major collection** reclaims it because the
Cheney scan never descends through a container that lies outside the from-set. `beam_rt` publishes the
roots correctly — and its own unscanned-root valve **would decline this very collection** (verified
below) — but the collection runs inside `beam_process::Process::deliver` and never consults it.
All instrumentation used to reach this conclusion has been removed from the tree.

## 1. Reproduction (deterministic)

`b13:go/0` writes `b13_in.erl` (`-module(b13_in). -export([f/0]). f() -> 1.`), reads it back and calls
`epp:parse_file(File, [], [])`, then prints `NFORMS=length(Forms)`.

```
erts_rust/target/debug/beam_rt -noshell run <dir>/b13.beam b13:go/0     # NFORMS=6  (bug)
erts_rust/target/debug/beam_rt -noshell -pa <dir> -eval 'b13:go()'      # NFORMS=5  (correct)
erl -noshell -pa <dir> -eval 'b13:go()' -s init stop                    # NFORMS=5  (oracle)
erts_rust/target/debug/beam_rt -noshell +hms 500 run <dir>/b13.beam b13:go/0   # NFORMS=5 (masked)
```

A small nursery (the default here) makes the divergence appear; the repro is deterministic. The
`-eval` route is a single non-scheduled pass and never diverges.

## 2. The corrupted data

The `epp` server's `#epp.erl_scan_opts` is built as
`[{text_fun, keep_ftr_keywords()}, {reserved_word_fun, ResWordFun}]` (`lib/stdlib/src/epp.erl:1048`)
and rides every `io:scan_erl_form/4` as the `Options` field of the
`{get_until, Encoding, erl_scan, tokens, Args, Options}` request. From request 4 on, that cons cell
stops being `[<ptr to {text_fun,F}> | <ptr to next cons>]` and reads as small integers — e.g.
`[small(2) | <within-heap list ptr>]` — so the file-server side no longer sees scan options and
`epp` produces one extra form.

## 3. Where the cell lives — and the `[base, top)` coverage verdict

The `Options` value at the send seam (`src/context/mod.rs::send_message_with_off_heap`) is the
`#epp.erl_scan_opts` field of the **receiving** process's state record, reached through that process's
own `x` registers / stack (`beam_process::Registers`) — a cell in its **mature** space.

Geometry of the cell at the collecting instant (one representative sample; the verdict is identical
on every sample):

```
cell_addr = 0x…e341f80                (stable; never relocated)
mature base = 0x…e341ee0   top = 0x…e342018   used = 39 words   capacity = 233 words
offset(cell) = (cell_addr - base)/8 = 20 words
verdict: off_words(20) < used(39)  ⇒  IN_LIVE   — IN RANGE = YES
```

**So the from-set range `Collector::collect_major` adds for the mature
(`from.add(&self.mature)`, `beam_gc/src/collector.rs:198` → `FromSet::add` pushes
`(space.base(), space.top())`, `beam_gc/src/space.rs`) DOES COVER the cell.** The
“cell at/after `top` ⇒ `FromSet::contains` false ⇒ `Target::Foreign`” hypothesis is **falsified**;
do not re-derive it. `Copier::classify_cons`/`classify_boxed` return `Target::Live` for *any*
reference **to** this cell.

## 4. The collecting window, and the runtime's own valve verdict

The killing collection is a single `deliver` into the epp process (pid 3):

```
PREDELIVER  to=pid3  base=0x…e341ee0  top=0x…e342018  used=39   young_spaces=1  roots=2053
            VALVE: foreign_latch=true  phase1_unscanned_roots=[0, 1, 1024, 1027]
            WATCH:   cell off_words=20 used=39 verdict=IN_LIVE
            REACH:   hits=3  roots_scanned=2053
POSTDELIVER to=pid3  base=0x…e33b9c0  top=0x…e33ba40  used=16   young_spaces=1
```

* `deliver` (`beam_process/src/process.rs:912`) → `ensure_capacity` (`:1171`) →
  `run_rooted_collection` (`:1232`) → `Collector::collect_major` (`beam_gc/src/collector.rs:195`),
  whose `self.mature = fresh` (`:226`) drops the old mature space. `minor_gcs` stays `0`; the mature
  base moves; live words 39 → 16. Afterwards the same stable address is found inside **another**
  process's mature — **dropped and recycled in place**, not relocated.
* It runs with the receiver `Waiting` and not relocation-exempt, so `has_unpublished_roots()`
  (`beam_process/src/process.rs:1097` = `exempt || state == Running`) is `false` and `deliver`'s gate
  (`:936`) admits the collection.
* **The runtime's own valve would have declined it.** `beam_rt`'s unscanned-root valve is
  `context/gc_walk.rs::any_root_is_unscanned` / `process_roots_are_unscanned`, whose always-run
  **phase 1** classifies every root word with `context/gc.rs::root_is_unscanned` (`:224`). Applied to
  the receiver's own root set at that instant it reports **four** unscanned root words
  (`x0, x1, stack0, stack3`) — the receiver's descent latch (`Runtime::foreign_reachable(pid)`) is
  **open** (`true`) for this pid at every delivery, not only at this one. So
  `process_roots_are_unscanned(_, pid3, true)` is `true`, i.e. the collection is one the valve classifies
  as **unsound**. The four valve-guarded sites are `context/gc.rs::{ensure_heap, ensure_heap_full,
  alloc_words_rooted}` and `Runtime::serve_gc_request`; the **fifth** collection site — the
  delivery-driven one inside `beam_process::Process::deliver` — consults **none** of them, and that is
  the whole bug.
* `beam_rt`'s admission doors produced no line in the window, so this node's valve is never consulted
  for this collection — the collection is entirely inside `beam_process`/`beam_gc`.

## 5. The mechanism (why the covered, rooted cell is still lost)

The roots gathered by `run_rooted_collection` **do reach** the cell (`hits=3, roots_scanned=2053`),
but every path's parent hop is an **out-of-from-set** container:

```
root(x0) = 0x…40cc0  [OTHER]  tuple(19)  ->  0x…e341f80  [MATURE] cons(TARGET)
root(x0) = 0x…576e0  [YOUNG]  tuple(4)   ->  0x…d1ff2c8  [OTHER] boxed -> 0x…57690 [YOUNG] tuple(6)
                                                        -> 0x…57670 [YOUNG] cons -> TARGET
```

`[OTHER]` = an address that is inside **neither** the receiver's nursery chain nor its mature space —
i.e. a region the collector does not scan (the runtime's pointer-stable arena / a pooled BIF heap / a
message heap fragment; exactly the regions `beam_rt::context::gc::root_is_unscanned` names). The
copier’s `impl EvacDest for Copier` (`beam_gc/src/copy.rs`) returns **`Target::Foreign`** for every
address `!self.from.contains(ptr)` (`classify_boxed` `:334`, `classify_cons` `:350`), and
`forward_slot` then leaves that reference **verbatim and does not follow it** — so the Cheney scan
(`complete`/`run`) never visits the foreign container's slots and never forwards the mature cell. To-
space therefore retains only the words declared live by the from-set *or reached from them*; the
reachable-but-foreign-parented mature cell is not copied, `self.mature = fresh` drops the block, the
address is recycled, and the `[small(2) | …]` corruption appears **in place at a stable address** —
exactly the observed symptom.

This also reconciles the earlier rounds: the cell is **reachable from the roots** (this round's walk,
hits=3) *and* not **traversed by the collector** (the earlier `in-mature = 0` walk), because the two
walks differ in whether they follow out-of-from-set containers.

## 6. What this rules out (this node stays clean)

* **Not a `beam_rt` publish/holder gap.** The offending root words are `pid3`'s own `x`/`stack` and
  they *are* published (`RootSet::ALL_LIVE` at the park arm, `runner/run.rs::publish_parked_roots`);
  the walk above proves the words are present and reach the cell. There is no missing holder.
* **Not the `[base, top)`/from-set coverage** — see §3; the cell is `IN_LIVE`.
* **Not the native `file_server`/`get_until` → `beam_io::io_device::Continuation` → `erl_scan:tokens/4`**
  path: it returns the right tokens and the right `eof`.
* **Not the message copier** (`beam_message::copy_struct`): plain cross-process sends of these shapes
  agree; the corruption is visible on the *sender's* own heap before the copy.
* **Not `beam_engine::Machine::args_scratch`**: `dispatch_wide`
  (`beam_engine/src/interpreter/bif.rs`) hands the buffer back **empty** after `call_bif`.

## 7. Report to the sibling owner (prose patch — NOT applied here; read-only siblings)

Invariant violated: **a from-set object that is reachable from a gathered root must be evacuated, even
when the reference is held by a container outside the from-set; equivalently, a collection must not be
run at all when the receiver's roots reach a container the collector cannot scan.**

1. **`beam_process` — the delivery-driven collection site (primary).**
   `Process::deliver` (`crates/beam_process/src/process.rs:912`) → `ensure_capacity` (`:1171`) →
   `run_rooted_collection` (`:1232`) runs a rooted collection with no “would this move be unsound?”
   predicate, unlike the host's four valve-guarded sites. Minimal patch in prose: give the site the
   host's refusal signal — either (a) extend the existing `has_unpublished_roots` gate (`:936`, flag at
   `:1097`) with a host-supplied “decline” predicate (e.g. a `Process` flag set by the host, or a
   closure parameter threaded from `ProcessTable::deliver`) that makes `deliver` return the raw
   `BeamError::Alloc(AllocKind::Heap|Message)` unchanged, so the host's fragment fallback
   (`beam_rt::context::RuntimeContext::send_fragmented_message`) delivers without moving a word; or
   (b) make `ensure_capacity`/`run_rooted_collection` refuse when the gathered roots reach a container
   outside the from-set — which `beam_process` cannot decide alone.
2. **`beam_gc` — the from-set/descent design (alternative/complementary).**
   `crates/beam_gc/src/collector.rs::collect_major` (`:195-198` from-set build, `:226`
   `self.mature = fresh`) + `crates/beam_gc/src/copy.rs::EvacDest for Copier` (`classify_boxed` `:334`,
   `classify_cons` `:350`; `forward_slot`) + `crates/beam_gc/src/live.rs`. Two candidate minimal
   patches, both conservative: (a) widen the from-set with a *scannable* range that includes the
   containers the engine legitimately holds (the Immix mature space already reports the whole
   `[base, base+capacity)` — see `AnyCollector::mature_contains` docs) — this requires the collector to
   be told about those regions; or (b) keep `Target::Foreign` but make the pre-scan descent follow a
   foreign container’s slots when the caller declares them safe to walk, so any from-set word reachable
   through them is forwarded.
   Sub-note for (b): “my parent is not in `from`” is **not** proof that I am garbage; the correct
   statement is only true when foreign containers cannot hold from-set pointers, which this runtime's
   arena/BIF-heap/fragment regions violate.

Note: a **`beam_rt`-only** mitigation exists and is cheap (use the runtime's existing lever): consult
`process_roots_are_unscanned(scratch, receiver, receiver_foreign_latch)` in
`context/mod.rs::deliver_message_with_off_heap_inner` **before** `self.process_table_mut().deliver(…)`
and, when it answers `true`, run that delivery with the **receiver** temporarily marked
`set_relocation_exempt(true)` (restoring the previous mark like `with_relocation_exempt` does for the
sender). `deliver` then returns the raw refusal and the ordinary fragment fallback delivers the
message, so no heap word moves under an unscanned container. It is a mitigation rather than the root
fix because a receiver whose roots ever reach an unscanned container would then never collect on the
delivery path (its heap grows; every engine admission for it already declines anyway), and because the
*correct* semantic (refuse a collection whose root set is unsafe) belongs to the collection site.

## 8. Regression test (blocked on the sibling fix)

`tests/compile_file.rs` must drive `epp:parse_file/3` (or `compile:file/2`) through the scheduled
`run` route on a **self-contained fixture under `tests/fixtures/`** and assert the observable result
(parsed form count 5, or a byte-identical produced `.beam`). `b13.erl` hard-codes an absolute path and
is therefore not committable as-is. The test is blocked until the collection site refuses an unsafe
root set (or the copier evacuates a from-set word through a foreign parent): with the bug present the
scheduled route yields 6, so the assertion cannot be made green from `beam_rt` alone.
