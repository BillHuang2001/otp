# A live mature cell reached only through an unscanned container is lost by a delivery-driven collection

Sibling-owned defect dossier for the A.9 **scheduled (`run`) route**. Present state; no history.
The observable symptom is an extra `{error,{4,epp,cannot_parse}}` form out of `epp:parse_file/3` (and
the downstream `erl_lint`/`compile:forms` `badarg`) on the scheduled route only — the `-eval` route is
a single non-scheduled pass and never diverges.

## 1. The violated invariant and the two sites that violate it
**A collection must not run against a root set that reaches a container outside the from-set, and a
from-set object reachable from a gathered root must be evacuated even when the reference to it is held
by such a container.**
* `beam_process::Process::deliver` (`crates/beam_process/src/process.rs:912`) → `ensure_capacity`
  (`:1171`) → `run_rooted_collection` (`:1232`) runs a rooted collection with **no** *is this move
  sound?* predicate — unlike every host-side collection site.
* `beam_gc`'s copier treats any pointer outside the from-set as `Target::Foreign`
  (`crates/beam_gc/src/copy.rs`, `impl EvacDest for Copier`: `classify_boxed` `:334`, `classify_cons`
  `:350`) and `forward_slot` then leaves that reference **verbatim and does not follow it**, so the
  Cheney scan never visits the foreign container's slots.
Consequence: `Collector::collect_major` (`crates/beam_gc/src/collector.rs:195`) builds its from-set
from the mature space (`:198`, `FromSet::add` pushes `(base, top)`), the scan retains only what the
from-set declares live *or reaches through from-set / collector-scanned objects*, and
`self.mature = fresh` (`:226`) drops the block of a still-live cell. The address is then recycled
**in place** (measured: the address is stable across successive deliveries while its contents change).

## 2. Measured evidence (one delivery into the `epp` server process, pid 3)
The killing collection is a **major** collection (`minor_gcs` stayed `0`; the mature base moved,
live words 39 → 16) inside a single `deliver`:

```
PREDELIVER  to=pid3  base=0x…e341ee0  top=0x…e342018  used=39   young_spaces=1  roots=2053
            WATCH:   cell off_words=20 used=39 verdict=IN_LIVE
            REACH:   hits=3  roots_scanned=2053
POSTDELIVER to=pid3  base=0x…e33b9c0  top=0x…e33ba40  used=16   young_spaces=1
```

**The cell is inside the from-set's range.** `offset(cell) = 20 words < used = 39` ⇒ `IN_LIVE`, so
`FromSet::contains` is `true` and `classify_cons`/`classify_boxed` return `Target::Live` for any
reference **to** the cell. The "cell at/after `top` ⇒ `Target::Foreign`" hypothesis is falsified; do
not re-derive it.

**The cell is reachable from the gathered roots** (`hits=3` over 2053 root words), but every path's
parent hop is an **out-of-from-set** container — a region the collector does not scan (the runtime's
pointer-stable arena, a pooled BIF heap, a message heap fragment; exactly the regions
`beam_rt::context::gc::root_is_unscanned` names):

```
root(x0) = 0x…40cc0  [OTHER]  tuple(19)  ->  0x…e341f80  [MATURE] cons(TARGET)
root(x0) = 0x…576e0  [YOUNG]  tuple(4)   ->  0x…d1ff2c8  [OTHER] boxed -> 0x…57690 [YOUNG] tuple(6)
                                                        -> 0x…57670 [YOUNG] cons -> TARGET
```

**The receiver is admitted by `deliver`'s own gate**: `has_unpublished_roots()`
(`crates/beam_process/src/process.rs:1097` = `relocation_exempt || state == ProcessState::Running`)
is `false` for a `Waiting`, non-exempt receiver, so the gate at `:936` lets the collection run.

**The corrupted data.** The `epp` server's `#epp.erl_scan_opts` is built as
`[{text_fun, keep_ftr_keywords()}, {reserved_word_fun, ResWordFun}]` (`lib/stdlib/src/epp.erl:1048`)
and rides every `io:scan_erl_form/4` as the `Options` field of the
`{get_until, Encoding, erl_scan, tokens, Args, Options}` request. From request 4 on that cons cell
reads as small integers (e.g. `[small(2) | <within-heap list ptr>]`), so the file-server side no
longer sees scan options and `epp` produces one extra form.

## 3. What this rules out
* **Not a `beam_rt` publish/holder gap.** The offending root words are the receiver's own `x`/`stack`
  and they *are* published (`RootSet::ALL_LIVE` at the park arm, `runner/run.rs::publish_parked_roots`);
  the walk above proves the words are present and do reach the cell.
* **Not the from-set geometry** — §2: the cell is `IN_LIVE`.
* **Not the native `file_server` `{get_until,…}` → `beam_io::io_device::Continuation` →
  `erl_scan:tokens/4`** path: it returns the right tokens and the right `eof`.
* **Not the message copier** (`beam_message::copy_struct`) and **not `beam_engine::Machine::args_scratch`**.

## 4. `beam_rt`'s mitigation (in place, conservative)
The runtime's own **two-phase unscanned-root valve** (`context/gc.rs::root_is_unscanned` +
`src/context/gc_walk.rs`, sticky per-pid latch `Runtime::foreign_reachable`) already classifies this
very collection as **unsound** — applied to the receiver's root set at that instant it reports four
unscanned root words. The four valve-guarded sites are
`context/gc.rs::{ensure_heap, ensure_heap_full, alloc_words_rooted}` and `Runtime::serve_gc_request`;
the delivery seam is now the **fifth** site that consults it:
`RuntimeContext::deliver_message_with_off_heap_inner` (`src/context/mod.rs`) calls
`delivery_collection_is_unsound(receiver)` before `ProcessTable::deliver` and, on `true`, runs the
delivery with the **receiver** marked relocation-exempt (`with_pid_relocation_exempt`), so `deliver`
returns its raw refusal and the ordinary heap-fragment fallback delivers the message — **no heap word
moves under a container the collector cannot scan**.
This is a mitigation, not the root fix: it only ever *declines* a collection, so a receiver whose roots
ever reach an unscanned container stops collecting on the delivery path and its heap grows there
(every engine admission for that process already declines anyway). The correct semantic belongs to the
collection site.

## 5. The patch owed to the sibling owners (prose; read-only siblings, not applied here)
1. **`beam_process` — the delivery-driven collection site (primary).** Give the site the host's
   refusal signal: either (a) extend the existing `has_unpublished_roots` gate (`process.rs:936`,
   flag at `:1097`) with a host-supplied *decline* predicate (e.g. a `Process` flag set by the host,
   or a closure parameter threaded from `ProcessTable::deliver`) that makes `deliver` return the raw
   `BeamError::Alloc(AllocKind::Heap|Message)` unchanged, so the host's fragment fallback delivers
   without moving a word; or (b) make `ensure_capacity`/`run_rooted_collection` refuse when the
   gathered roots reach a container outside the from-set — which `beam_process` cannot decide alone.
2. **`beam_gc` — the from-set/descent design (alternative/complementary).**
   `collector.rs::collect_major` (`:195-198` from-set build, `:226` `self.mature = fresh`) +
   `copy.rs::EvacDest for Copier` (`classify_boxed` `:334`, `classify_cons` `:350`; `forward_slot`) +
   `live.rs`. Two conservative candidates: (a) widen the from-set with a *scannable* range that
   includes the containers the engine legitimately holds (the Immix mature space already reports the
   whole `[base, base+capacity)` — see `AnyCollector::mature_contains` docs), which requires the
   collector to be told about those regions; or (b) keep `Target::Foreign` but make the pre-scan
   descent follow a foreign container's slots when the caller declares them safe to walk, so any
   from-set word reachable through them is forwarded.
   Sub-note for (b): *my parent is not in `from`* is **not** proof that I am garbage — that is only
   true when foreign containers cannot hold from-set pointers, which this runtime's
   arena/BIF-heap/fragment regions violate.

## 6. Regression test (in place)
`tests/compile_file.rs::a_scheduled_process_parses_a_source_file_like_the_oracle` drives
`epp:parse_file/3` through the scheduled `run` route over the committable fixtures
`tests/fixtures/epp_forms.erl` + `tests/fixtures/epp_input.erl` (both `include_str!`-embedded, scratch
dir from `std::env::temp_dir()`) and asserts the observable parsed form list
(`{5,[attribute,attribute,attribute,function,eof]}`). It failed before the §4 mitigation (`exit 1 —
beam_rt: badarg`) and passes after.

## 7. Refutation of the sweep-driven hypothesis (raw experiment, this node)
The §2/§5 claim that a **delivery-driven** `Process::deliver` → `ensure_capacity` collection is the
mover is **refuted** by a decisive experiment: forcing `RuntimeContext::delivery_collection_is_unsound`
(`src/context/mod.rs:1020`) to answer `true` for **every** receiver — so every delivery runs with the
receiver marked relocation-exempt and `Process::deliver` returns its raw refusal to the heap-fragment
fallback, moving **no** word — did **not** stop the corruption. The reproducer still died:
`beam_term::ops: invalid boxed header subtag 0x12` (interpreter tier; `run_label_blocking` ←
`FunBridge::call_fun` ← `beam_bifs::lists::foldl3`; baseline tier inside
`beam_engine::jit::baseline::runtime::jit_step_one` ← `jit_ext_call`).
A heap-move tracer over this crate's own seams (`call_bif` / `run_export_blocking` /
`run_label_blocking` / `deliver_into_mailbox`, plus a `DOOR` line at `context/gc.rs::collect_or_grow`)
then showed that **no `beam_rt` admission-point collection runs at all** during the corrupting run
(`collect_or_grow` never fired) and the corrupting process's (pid 4) nursery **base never changes**
across the deliveries that fill its mailbox. So the loss is **not** a `beam_process::Process::deliver`
relocation and **not** an `ensure_heap`/`ensure_heap_full` relocation: the failing read is a **stale
word read from a heap position re-used/recycled** by a collection elsewhere (a live holder — Rust-local
term handles in a `beam_bifs`/`beam_engine` frame, or an unpublished `Machine`/engine-observable field
— reaching a from-set word), not a base-move of the reader's own heap.
Suppressing collection entirely (`+hms 4194304`) still makes both shapes print `1` / exit 0 on both
tiers, so a collection IS the loss; it is simply not one of the sites §2/§5 named.
Consequence: §5's patch to `beam_process` (the `has_unpublished_roots` decline predicate) addresses a
real but **different** hazard; it is not the fix for the A.9 selfhost mismatch. The mover is still
unpinned; the next pass must locate the collection that actually recycles the word the reader holds.
