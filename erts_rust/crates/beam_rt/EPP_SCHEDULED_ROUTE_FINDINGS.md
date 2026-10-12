# `epp` on the scheduled (`run`) route — localization findings

Status: **NOT fixed in `beam_rt`.** `beam_rt` publishes the epp process's roots correctly — the
corrupted cell is **reachable from the process's own published `x`/`stack`** at the collecting seam —
yet a **delivery-driven major collection** of the receiving process reclaims it. The defect is
therefore in a sibling crate (`beam_gc`'s major from-set/descent and/or `beam_process`'s
`deliver`-driven collection gate), **not** in this node. All instrumentation used to reach this
conclusion has been removed from the tree; nobody should re-add it.

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

## 3. Where the value comes from, and where the cell lives

The `XtraArgs[1]` `Options` value at the send seam (`src/context/mod.rs::send_message_with_off_heap`)
originates in the **epp process's own heap** (pid 3): it is the `#epp.erl_scan_opts` field of the
process's state record, reached through the process's `x` registers / stack — a
`beam_process::Registers` region, i.e. one of the families `Process::run_rooted_collection`
(`beam_process/src/process.rs:1232`) gathers.

`LOCATE` of the watched cell at the seam is decisive:

```
LOCATE reply watched …-opts addr=0x…ff80 -> mature(ProcessId { number: 3 })   # the cell lives in pid3's MATURE
```

and it is reachable, at the seam **immediately before the collecting `deliver`**, from pid3's **own
young heap** words held in its published registers:

```
PATH tag=reply-target-x    who=pid3  firsthop=0x…a1b0 -> cell …-opts @0x…ff80
LOCATE reply-target-x firsthop(…-opts) addr=0x…a1b0 -> young(ProcessId { number: 3 })
PATH tag=reply-target-stack who=pid3 firsthop=0x…a198 -> cell …-opts @0x…ff80
LOCATE reply-target-stack firsthop(…-opts) addr=0x…a198 -> young(ProcessId { number: 3 })
```

So the first hop of the path to the mature cell is a **young, process-heap** object (not a
literal, not off-heap, not the arena), rooted in `x`/`stack`. Both ends are inside the major
collection's from-set (`Collector::collect_major`, `beam_gc/src/collector.rs:195-198`,
`from.add(&self.mature)`), so a correct Cheney copy from those roots must keep it.

## 4. The collecting window

Across the request that finally delivers the corrupted payload:

```
GC serve-drain pid=pid3 state=Waiting min=0 maj=2 young_avail=35 mature_avail=217
…  (REPLY-DELIVER, then the next pass) …
GC serve-drain pid=pid3 state=Waiting min=0 maj=3 young_avail=210 mature_avail=196
CHANGED tag=before-reply-deliver …-opts now=[other@0x0|i2]
```

* The collection runs **inside `Process::deliver`** — `deliver` (`beam_process/src/process.rs:912`)
  → `ensure_capacity` (`:1171`) → `run_rooted_collection` (`:1232`) → `Collector::collect_major`
  (`beam_gc/src/collector.rs:195`), whose `self.mature = fresh` (`:226`) **drops the old mature
  space**. `minor_gcs` stays `0`; the mature base moves.
* It runs with the receiver `Waiting` and not relocation-exempt, so `has_unpublished_roots()`
  (`beam_process/src/process.rs:1097` = `exempt || state == Running`) is `false` and `deliver`'s gate
  (`:936`) admits it.
* After the window the same address is found inside **another** process's mature space
  (`LOCATE reply watched …-opts addr=0x…ff80 -> mature(ProcessId { number: 2 })`), i.e. pid3's old
  mature was released and its memory re-used — the address is stable while the contents change,
  so nothing was *relocated*, it was **dropped and recycled in place**.
* `beam_rt`'s four admission doors (`context/gc.rs::{ensure_heap, ensure_heap_full,
  alloc_words_rooted}`, `Runtime::serve_gc_request`) produced no line in the window, so this node's
  `root_is_unscanned` valve (`context/gc.rs:224`) is never consulted for this collection — the
  collection is entirely inside `beam_process`/`beam_gc`.

## 5. Verdict on the discriminator question

> Is the corrupted mature-resident cell reachable by **graph traversal (depth > 1)** from the root
> set `run_rooted_collection` gathers at the collecting moment?

**REACHABLE — from the process's own published `x`/`stack`, through a young process-heap object.**
An earlier round reported `in-mature=0` from a walk run *at the collecting site*; this round's walk
from the same accessor (`Process::registers()`, i.e. exactly what `run_rooted_collection` gathers)
finds concrete paths with young, in-heap first hops. The two only reconcile if the roots the
collector actually consumed at that instant did **not** contain those `x`/`stack` words, or if the
copy dropped a from-set object reached through them.

Because `beam_rt` publishes those words (they are present and pointing at the cell immediately
before the collecting `deliver`, and `deliver` does not mutate the receiver's `regs`), the omission
is **not** a `beam_rt` publish gap. The owner is one of the two sibling crates below.

## 6. What this rules out (this node stays clean)

* **Not the native `file_server`/`get_until` → `beam_io::io_device::Continuation` →
  `erl_scan:tokens/4`** path: it returns the right tokens and the right `eof`.
* **Not the message copier** (`beam_message::copy_struct`): plain cross-process sends of these shapes
  agree; the corruption is visible on the *sender's* own heap before the copy.
* **Not `beam_engine::Machine::args_scratch`**: `dispatch_wide`
  (`beam_engine/src/interpreter/bif.rs`) hands the buffer back **empty** after `call_bif`.
* **Not this node's park/resume publication**: `publish_parked_roots`
  (`src/runner/run.rs:97`) publishes `RootSet::ALL_LIVE` (x + whole stack + FR) plus the
  pending-exception reason through `extra_roots`; the walk above proves the words are there.
* **Not a `beam_rt`-owned Rust store**: the path's first hop is a young **process-heap** object, not
  an arena/`bif_heaps`/`persistent_term` term.

## 7. Report to the sibling owner (prose patch — NOT applied here; read-only siblings)

The invariant violated is: *a collection may only reclaim a word that is not reachable from the
roots it gathered; and if a from-set object is reachable from a root, its whole reachable transitive
closure within the from-set must be evacuated.*

1. **`beam_gc` — `Collector::collect_major` (`crates/beam_gc/src/collector.rs:195-226`)** and the
   copier's from-set classification (`crates/beam_gc/src/copy.rs::EvacDest::classify_boxed`
   `:334-348`, `classify_cons` `:350-361`) plus the Cheney scan (`forward_roots`/`scan` `:389+`).
   The from-set is `nursery + mature` and a young→mature reference must be followed and copied. If
   the scan stops at a boundary or the from-set build misses a retained nursery space
   (`Nursery::add_to_from_set`), a reachable mature object is dropped. Minimal check: assert that
   every word whose `classify_*` returns `Target::Live` is actually reserved+forwarded, and that the
   scan revisits the whole copied closure.
2. **`beam_process` — `Process::deliver` (`:912`) → `ensure_capacity` (`:1171`) →
   `run_rooted_collection` (`:1232`)** with the `has_unpublished_roots` gate (`:1097`, used at
   `:936`). If the roots gathered at this instant do not actually contain the receiver's live
   `x`/`stack` (e.g. a stale `regs` snapshot for a `Waiting` process whose machine's registers live
   elsewhere), then the gather is incomplete and the collection must be refused (or the roots
   refreshed) before `collect_major` runs.

The evidence above (a young in-heap first hop rooted in published `x`/`stack`, dropped by a
`deliver`-driven `collect_major`) points first at **(1)**; **(2)** is the alternative if the roots
consumed at the collecting instant turn out to be a stale snapshot.

## 8. Regression test (blocked on the sibling fix)

`tests/compile_file.rs` must drive `epp:parse_file/3` (or `compile:file/2`) through the scheduled
`run` route on a **self-contained fixture under `tests/fixtures/`** and assert the observable result
(parsed form count 5, or a byte-identical produced `.beam`). `b13.erl` hard-codes an absolute path and
is therefore not committable as-is.
