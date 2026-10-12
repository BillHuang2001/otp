# `epp` on the scheduled (`run`) route — localization findings

Status: **NOT fixed in `beam_rt`.** The divergence is a **delivery-driven major collection that
drops live mature-resident state of the receiving process**; the corrupted term is **not reachable
by graph traversal from the root set that collection gathers**. The defect is therefore in a sibling
crate (`beam_process` root gathering / its `has_unpublished_roots` gate, and/or the `beam_gc` major
collection's from-set descent) — **not** in this node. All instrumentation used to reach this
conclusion has been removed from the tree.

## 1. Reproduction (deterministic)

`b13:go/0` writes `b13_in.erl` (`-module(b13_in). -export([f/0]). f() -> 1.`), reads it back and calls
`epp:parse_file(File, [], [])`, then prints `NFORMS=length(Forms)`.

```
erts_rust/target/debug/beam_rt -noshell run <dir>/b13.beam b13:go/0     # NFORMS=6  (bug)
erts_rust/target/debug/beam_rt -noshell -pa <dir> -eval 'b13:go()'      # NFORMS=5  (correct)
erl -noshell -pa <dir> -eval 'b13:go()' -s init stop                    # NFORMS=5  (oracle)
erts_rust/target/debug/beam_rt -noshell +hms 500 run <dir>/b13.beam b13:go/0   # NFORMS=5 (masked)
```

A small nursery (`+hms 233`, the default here) is what makes the divergence appear; the repro is
deterministic. The `-eval` route is a single non-scheduled pass and never diverges.

## 2. The corrupted data

The `epp` server's `#epp.erl_scan_opts` is built as
`[{text_fun, keep_ftr_keywords()}, {reserved_word_fun, ResWordFun}]` (`lib/stdlib/src/epp.erl:1048`)
and rides every `io:scan_erl_form/4` as the `Options` field of the
`{get_until, Encoding, erl_scan, tokens, Args, Options}` request. That cons cell degrades in place:
its two words stop being `[<ptr to {text_fun,F}> | <ptr to next cons>]` and become **small
integers** — e.g. `[small(2) | <within-heap list ptr>]` — so the file-server side no longer sees scan
options and `epp` produces one extra form.

## 3. The decisive measurement — reachability at the collecting moment

The probe gathered, at the delivery seam, exactly the root families that
`Process::run_rooted_collection` (`beam_process/src/process.rs:1232`) gathers — `regs.gather_roots`
(x + whole stack + `extra_roots`), mailbox `payload`/`from`/`token`, `dict`, `saved_args`, `signals` —
and performed a **full graph walk** (every LIST/BOXED non-literal pointer, `beam_term` accessors,
word budget) recording the set of reached object addresses.

Raw log (probe run, `BEAM_RT_EPP_PROBE=1 … run b13.beam b13:go/0`), around the collecting seam:

```
[probe:before-rx] pid=3 state=Waiting exempt=false unpub=false young=[0x…3c120..+233] mature=[0x…41ee0..+39] minor_gcs=0 major_gcs=1
[probe:before-rx] roots n=2053 reg=2053 mail=0 in-young=3 in-mature=0 reached-objects=197
[probe:after-rx]  pid=3 state=Runnable exempt=false unpub=false young=[0x…3c120..+4]  mature=[0x…3b9c0..+16] minor_gcs=0 major_gcs=2
[probe:after-rx]  roots n=2056 reg=2053 mail=3 in-young=1 in-mature=3 reached-objects=199
```

* The collection ran **inside `Process::deliver`** (the receiver's `state` was `Waiting` before and
  `Runnable` after in the same seam; `minor_gcs` stayed `0` while the mature **base changed** from
  `0x…41ee0` to `0x…3b9c0`), i.e. `ensure_capacity` → `run_rooted_collection` →
  `Collector::collect_major` (`beam_gc/src/collector.rs:195`, `from.add(&self.mature)` /
  `self.mature = fresh` at `:226`).
* It ran with `exempt=false` and `has_unpublished_roots()==false`
  (`beam_process/src/process.rs:1097` = `exempt || state == Running`), so `deliver`'s gate
  (`:936`) admitted it.
* **The gathered root families hold no word pointing into the mature space at all**
  (`in-mature=0`) — while the mature space held 39 live-looking words that the same collection then
  reduced to a 16-word promoted-only space. Nothing in the gathered set reaches them.

`beam_rt`'s four admission doors (`context/gc.rs::{ensure_heap, ensure_heap_full,
alloc_words_rooted}`, `Runtime::serve_gc_request`) produced **no** line in the divergence window, so
this node's `root_is_unscanned` valve (`context/gc.rs:224`) is never consulted for this collection —
the collection is entirely inside `beam_process`/`beam_gc`.

## 4. Verdict on the discriminator question

> Is the corrupted mature-resident cell reachable by **graph traversal (depth > 1)** from the root
> set `run_rooted_collection` gathers at the collecting moment?

**NOT REACHABLE.** The direct-root census is `in-mature=0` at the collecting seam and the full walk
adds no path back into the mature base that is about to be replaced, yet live terms of the receiving
process live there and are used after the collection. The address of the degraded cell is **stable
across requests while its contents change**, which rules out "the collector relocated it and a stale
pointer was left behind" — the collector does not move it (it never forwards it), so a later
allocation reuses the freed block and overwrites the cell in place.

⇒ With an incomplete root set at a collecting site that `beam_rt` does not drive, the owner is the
sibling crate (`beam_process` gathers/gates, `beam_gc` copies). Reported to the parent node; **no
sibling file was edited** (out of this node's write scope).

## 5. What this rules out (this node stays clean)

* **Not the native `file_server` / `get_until` → `beam_io::io_device::Continuation` →
  `erl_scan:tokens/4`** path: it returns the right tokens and the right `eof`; the same path driven
  from the main scheduled process agrees with the oracle.
* **Not the message copier** (`beam_message::copy_struct`): plain cross-process sends of these shapes
  agree.
* **Not this node's park/resume publication**: `publish_parked_roots` (`src/runner/run.rs:97`)
  publishes `RootSet::ALL_LIVE` (x + whole stack + FR) plus the pending-exception reason through
  `extra_roots`, and `scatter_roots_full`/`resync_parked_roots` (`src/context/mod.rs:681`) reconcile
  the machine on resume. The seam is exercised by the observed delivery-driven collection, and the
  gathered set is the same one the collection consumed.

## 6. Proposed minimal patch (prose only — owned by the sibling crates, NOT applied here)

The invariant violated is: *a collection may only run when its gathered root set covers every live
heap word of the collected process.* Two candidate minimal closures:

1. **`beam_process` (`Process::deliver` → `run_rooted_collection`, `src/process.rs:1232` /
   `has_unpublished_roots` at `:1097`)** — the gate that admits the delivery-driven collection
   (`:936`) tests `relocation_exempt || state == Running`, which cannot see a reference the gathered
   families do not contain. Widen the gathered set with the missing family (whatever holds the
   receiver's live mature state), or make the delivery-driven site obey the same
   `root_is_unscanned`-style valve the host admission doors use before it is allowed to `collect_major`.
2. **`beam_gc` (`Collector::collect_major`, `src/collector.rs:195-198`, with the from-set
   classification in `src/copy.rs::EvacDest::classify_boxed` at `:334-353`)** — if the start of the
   difference is on the collector side, the invariant is that the from-set must be the *union* of the
   spaces a root can name; a mature object reached only through a chain whose head is in the other
   space must be copied, never dropped.

Which of the two is minimal depends on whether the receiver's gathered families are in fact missing a
region (then (1)) or the collector's from-set build excludes it (then (2)); the raw evidence above
(`in-mature=0` at the seam) points first at (1).

## 7. Regression test (blocked on the sibling fix)

`tests/compile_file.rs` must drive `epp:parse_file/3` (or `compile:file/2`) through the scheduled
`run` route on a **self-contained fixture under `tests/fixtures/`** and assert the observable result
(parsed form count 5, or a byte-identical produced `.beam`). `b13.erl` hard-codes an absolute path and
is therefore not committable as-is.
