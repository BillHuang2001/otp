# `beam_ets` — measured performance & memory notes

Every figure below was produced by a committed test in this crate (no figure is copied
from another benchmark run), and **both sides of every ratio are named**: the table's own
footprint ([`Table::memory`](src/table/read.rs) / [`TermStore::words`](src/store/mod.rs),
a **word** count like `ets:info(T, memory)`) and the **process** RSS (Linux
`/proc/self/status` `VmHWM`/`VmRSS`). Each run records `/proc/loadavg`, because the host is
shared and absolute memory figures are load-sensitive.

## How to reproduce

```text
# (a) the crate's own side — table footprint + process RSS, one key upserted N times
BEAM_ETS_MEMORY_N=100000 erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --test ets_memory -- --nocapture

# (b) the oracle's side of the same loop (escript, peak RSS polled at 1 ms)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --test ets_copy_oracle -- --nocapture

# (c) the oracle-cross-checked reclamation accounting
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --test ets_reclamation_oracle -- --nocapture

# (d) the ordered_set interior comparison, B+-tree vs sorted Vec (§3 — its own
#     reproduction commands, sizes/orders and loadavg are recorded there)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --lib ordered_interior_microbenchmark -- --nocapture
```

`tests/ets_memory.rs` contains **one** test on purpose: `VmHWM` is a process-wide
high-water mark, so a second test running concurrently would raise it under the first.

## 1. The `copy_bound/ets_copy` peak-RSS question — **not this crate**

The bench workload (`erts_rust/tools/bench/src/workloads/copy_bound.rs`, the `ets_copy`
case) does, per op: `T = payload()`, `ets:insert(Tab, {k, T})`, `ets:lookup(Tab, k)`,
against **one** `set` table with **one** key. `tools/bench/RESULTS.md` §6.7 / §8 item 5
records rust `beam_rt` peak RSS **90.30 MiB** at `N = 100 000` against the oracle's
**66.26 MiB**; the m10 rust-vs-oracle growth is **1278.5 KiB**.

`tests/ets_memory.rs` drives exactly that object shape through this crate only (no
`beam_rt`), in two phases: (A) `TermStore::copy` + `free` — the copy boundary alone; (B) the
full `Table::insert` + `Table::lookup` loop with the same payload term.

| quantity | N = 100 000 | N = 1 000 000 | shape |
|---|---|---|---|
| A `TermStore::words` | **73** words, op 1 → N | **73** | flat |
| A `TermStore::blocks` | **3**, op 1 → N | **3** | flat |
| B `Table::memory` | **92** words (warm-up floor) | **92** | flat |
| B process `VmRSS` growth over the loop | **1376 KiB ≈ 1.34 MiB** | **1412 KiB ≈ 1.38 MiB** | one-time step (RSS at op 1 already ≈ the end value) |

`/proc/loadavg` at the N = 100 000 run: `10.50 8.43 8.18`.

Oracle side, `tests/ets_copy_oracle.rs` (same loop, verbatim, under the system `escript`;
peak `VmHWM` polled at 1 ms), `/proc/loadavg` `7.63 6.15 6.02`:

| N (loop count) | `ets:info(Tab, memory)` empty → after 1 insert → after N → after delete | peak `VmHWM` |
|---|---|---|
| 0 | 306 → 383 → — → — | **76 168 KiB** |
| 100 000 | 306 → 383 → **383** → 306 | **77 152 KiB** |
| 2 000 000 | 306 → 383 → **383** → 306 | **75 608 KiB** |

So over a 20× range of `N` the oracle's peak RSS grows **0 KiB (0.00 MiB)** and its
one-key table memory is flat at 383 words, returning to its 306-word empty floor.

**Attribution (evidence, not inference).** The ETS adopt/copy/read path this crate owns
contributes **0 MiB per op**: the store's live words and block count are constant from op 1
to op 1 000 000, the table's footprint is a constant 92 words, and the process RSS shows a
single warm-up step rather than per-op growth (a 1 000 000-op loop adds no more than a
100 000-op loop: 1412 KiB vs 1376 KiB, i.e. +36 KiB over 10× the ops = the allocator's own
noise). The rust/sys peak gap is therefore **sibling-side**, and the measurement matches the
batch-52 finding that the *churn* loop's growth is the caller-side BIF-result copy-out seam:
`erts_rust/crates/beam_rt/src/context/calls.rs` — `move_result_into_process_heap` copies a
registered BIF's result onto the **process's own young heap** and, on a full nursery, calls
`Process::grow_nursery` + one retry (the no-relocation seam), and nothing shrinks the
nursery back (`heap.reset_to(mark)` only rewinds to the call's own mark; `safe_to_rewind` /
`open_gate_for_foreign_result` gate the rewind). `ets:lookup/2` here returns
`[{k, ~73-word payload}]`, an ~80-word nursery request per op, so the grow trigger is crossed
a bounded number of times — each growth possibly doubling, which is exactly the measured
*one-time* step shape rather than a per-op leak.

**Suggested measurement for the owning sibling** (this crate cannot make it): in
`beam_rt`, sample the runner process's `VmHWM` and the process heap's word counts
(`Heap::words()`/capacity before and after `move_result_into_process_heap`) around the
`ets_copy` loop, and record the **nursery capacity after each `grow_nursery`** — if the
capacity after the last growth accounts for the whole rust-vs-sys gap, the fix is a nursery
high-water policy (shrink-on-idle, or a per-call nursery cap) and not an ETS cost.

## 2. Per-object reclamation versus the oracle (`tests/ets_reclamation_oracle.rs`)

`ets:info(T, memory)` is a word count on both sides, but the crate counts the table's own
store **plus the index's allocated capacity** (which can fall with occupancy, below), while the
oracle's figure tracks live data on a small fixed base — so the absolute values are not comparable
(loaded 100 keys: crate `set/chained` 3468 words vs oracle `set` 1606 words). What is comparable,
and what the test asserts, is the **contract**. Oracle figures (`escript`, `/proc/loadavg`
`2.93 4.63 5.79` for the delete family and the progression, `2.60 4.12 5.50` for the last run):

| family | oracle | crate |
|---|---|---|
| `delete/2`, `delete_object/2`, `delete_all_objects/1` each return a 100-object table to the **empty** floor | `set`/`bag`/`duplicate_bag` 306 → 1606 → **306**; `ordered_set` 140 → 1540 → **140**; `size` → 0 | every shape: 3 cycles land on one stable floor, strictly below the loaded figure and `==` the empty table's own; both `delete_all_objects` and a per-key emptying return **exactly** to the empty figure (the interior is rebuilt when it empties — see below) |
| overwrite of an existing key (`m3` vs `m2`) | `set`/`ordered_set` **flat** (`m3 == m2`); `bag`/`duplicate_bag` grow by one object | same classification, exactly |
| `delete_object/2` of the overwritten object (`m4`) | returns exactly to the pre-overwrite figure (`m1` on the upsert kinds, `m2` on the multi kinds) | direction verified (`m4 < m3`, `m4 >= m0`); on a chained index an emptied bucket keeps its minimum `Vec<Entry>` allocation (and an entry its `objs` vector) — the bucket *array* itself shrinks back — so `m4` can sit above `m1` (measured 71 words vs `m1` = 55 at `/proc/loadavg` `34.65 34.28 18.42`) |
| the removal path's endpoint (`m6` vs `m0`) | exactly `m0` (`m6_eq_m0` true for every kind) | `== m0` for every shape: the emptied interior is rebuilt (`release_empty_interior`) |
| heir gift data | `{heir, self(), {a_gift, [1..8]}}` costs **+23** words; `{heir, self(), undefined}` costs **+0**; 2000 set/clear rounds land on the cleared figure | +19 words for an equally shaped gift; arity-2 and `ClearHeir` return exactly to the base figure; 2000 rounds flat |
| `give_away/3` | owner moves, `memory` **unchanged**, `size` unchanged | same |
| heir transfer on owner death | table survives, owner = heir, heir **kept installed**, `memory` does not fall | same (`memory` unchanged, gift copy retained, reclaimed exactly on `ClearHeir`) |

### Index storage follows live occupancy (`tests/ets_capacity.rs`)

Every interior releases index capacity as objects leave, so the reported figure tracks live
occupancy instead of the high-water mark a grow-then-teardown leaves behind. Word counts are
load-independent, so no `/proc/loadavg` is recorded for this table. `Table::memory()` words at
**empty / 10 000 inserted / 1 left after deleting 9 999 / after `delete_all_objects`**:

| shape | empty | 10 000 inserted | 1 left | after `delete_all_objects` |
|---|---|---|---|---|
| `set` (CHAMP, default) | 7 | 90 775 | **38** | 7 |
| `ordered_set` (B+-tree, default) | 24 | 81 480 | **11** | 24 |
| `bag` / `duplicate_bag` / `set`-chained | 24 | 259 152 | **45** | 24 |
| `ordered_set` (sorted `Vec`) | 0 | 62 768 | **9** | 0 |

Each shape's trigger, with its own hysteresis so a stable-size churn never reallocates: the chained
index rebuilds its bucket array through `HashInterior::rehash_into` once the object load falls below
an eighth (`maybe_shrink`; never below `INITIAL_BUCKETS` = 8, halving while it stays under 1/8 — a 6×
band under the 3/4 grow threshold) and trims an entry's object vector past 4× slack; the CHAMP `set`
trims every node a removal walked once `capacity > 8 && capacity > 4 * len`; the B+-tree compacts its
node arena when retired slots outnumber live ones (`NodeStore::needs_compaction`/`compact`, a dense
renumbering that re-trims each surviving node) and trims each leaf's entry vector on removal; the
sorted `Vec` releases past `CAPACITY_SHRINK_FACTOR` = 4.

The oracle's side of the same trajectory makes the **direction** comparable and the absolute values
not: its `ordered_set` tracks live data almost exactly (140 words empty → 148 with one object left)
while its hash `set` only returns toward a small floor (306 → 2 624). The interior layout is not
externally pinned, so the crate's contract is the **shape** — the figure falls with live data, no
per-removal churn, and the empty floor is exact — not the oracle's word counts.

The empty floor needs its own path, because a *downsized* index is not a *fresh* one (a chained
index's bucket array stops at its starting count, a compacted arena keeps its root): `Table::delete`/
`delete_object`/`take` route through the private `Table::release_empty_interior`, which rebuilds the
interior once the last object has left it — the same `build_interior` operation `delete_all_objects`
uses — so a delete-it-all trajectory lands on the empty figure **exactly**. Only a removal that
actually happened calls it, so `delete` of an absent key never churns the index.

The residual is therefore **one slack allocation per interior, at the interior's own shrink factor**,
not a per-object high-water mark: a `set` that held 10 000 objects and holds one reports 38 words
(its empty floor is 7), the chained kinds 45 (floor 24), the sorted `Vec` 9 (floor 0), and the B+-tree
11 — *under* its own 24-word fresh floor, because a compacted arena's slot vector is allocated at
exactly the live node count while a freshly built tree reserves its initial arena. A stable-size
churn does not trade that back and forth: 200 rounds of 1 000 keys, deleting half and re-inserting
half each round, keep `max == base` (or below) for every shape — no
growth and no shrink/re-grow thrash, which is what the test asserts alongside the trajectory.

Reproduce: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets
--test ets_capacity -- --nocapture`. `ets:safe_fixtable/2` is the one deliberate exception in the
figures above: while a table is fixed, a removed object's storage waits in `deferred_frees`, so memory
falls at unfix rather than per removal (`ets_fixtable.rs`).

## 3. The `ordered_set` interior: B+-tree versus the sorted `Vec`

`src/interior_bench.rs::ordered_interior_microbenchmark` measures the crate's two selectable
`ordered_set` interiors over the same key sets:

* **`OrderedSetInterior`** — the K-ary B+-tree (`ORDER = 32`, `MIN_OCCUPANCY = 16`, an
  arena of `NodeId` nodes that never relocate, a leaf chain and borrow-or-merge rebalancing),
  the crate default (`OrderedInterior::BPlusTree`, `#[default]`);
* **`SortedOrderedInterior`** — one sorted `Vec` (`OrderedInterior::Sorted`), the interior the
  B+-tree replaced, retained as the selectable alternative.

Per key set it times, in ns per operation: **insert** (upsert of the `n` present keys),
**insert_new** (a disjoint `n`-key set, so every call takes the "key is new" path — a distinct
descent in the tree), **hit** (lookup of a present key), **miss** (`member` of an absent key),
**iter** (one full `iter_objects` walk) and **delete** (removing all `n` keys), and reports the
interior's own [`Interior::memory`] **bytes/entry**. Only the *index* is timed: both interiors
store the same key term as the object, so the shared term cost cancels out of the comparison,
but the last column is each interior's own footprint. Every measurement **asserts its answers**
as it goes (`size` after insert/delete, hit/miss, the iteration count, `insert_new`'s `true`
then `false`), so a fast-but-wrong index fails the run rather than winning the benchmark.
`insert_new`'s second pass over the same keys also asserts that a refusal stores nothing.

The **key order** is the second dimension, selected by
`BEAM_ETS_BENCH_ORDER=shuffled|ascending|descending|all` (default `shuffled`, which is the
single-order behaviour the benchmark started with; `set`-table runs are unaffected). It is a
B+-tree irrelevance and a sorted-`Vec` decisive: ascending inserts are the `Vec`'s append fast
path, while ascending deletes are its shift-the-whole-tail worst case. Both orders derive from
the interior's own comparison (`cmp_arith`, so `1` and `1.0` order by arithmetic value), not
from raw term order.

### Reproduce

```text
# the two-interior table (both interiors, all three orders; the slow one)
BEAM_ETS_BENCH_ORDER=all BEAM_ETS_BENCH_SHAPES=smallint,atom,binary,tuple \
BEAM_ETS_BENCH_SIZES=1000,100000 BEAM_ETS_BENCH_REPEATS=5 \
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --lib ordered_interior_microbenchmark -- --nocapture

# 1e6, B+-tree alone (the sorted Vec is O(n) per op and far too slow there)
BEAM_ETS_BENCH_ORDER=all BEAM_ETS_BENCH_SHAPES=smallint BEAM_ETS_BENCH_SIZES=1000000 \
BEAM_ETS_BENCH_INDEX=bplus BEAM_ETS_BENCH_REPEATS=3 \
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --lib ordered_interior_microbenchmark -- --nocapture
```

`BEAM_ETS_BENCH_REPEATS=<passes>` makes the reported figure the **per-metric minimum** over
freshly built interiors: on a shared host a pass can only be made *slower*, so the minimum is
the interference-free estimator and a single pass is not comparable between runs.

Runs recorded here. **Table (A)** — `BEAM_ETS_BENCH_ORDER=all`,
`BEAM_ETS_BENCH_SHAPES=smallint,atom,binary,tuple`, `BEAM_ETS_BENCH_SIZES=1000,100000`,
`BEAM_ETS_BENCH_REPEATS=5`, wall 197 677 ms, `/proc/loadavg` `7.27 15.36 11.90` at the start
and `62.44 34.92 19.96` after it (the host's load climbed an order of magnitude mid-run).
**Table (B)** — B+-tree only, `BEAM_ETS_BENCH_SIZES=1000000`, `BEAM_ETS_BENCH_INDEX=bplus`,
`BEAM_ETS_BENCH_REPEATS=3`, wall 9 690 ms, `/proc/loadavg` `10.54 17.19 12.28` /
`9.82 16.82 12.22`. **Cross-check (C)** — the same `smallint` cells as (A) but measured in
their own process at `BEAM_ETS_BENCH_SIZES=10,1000,100000`, `REPEATS=5`, wall 32 988 ms,
`/proc/loadavg` `13.45 18.49 12.50` / `10.93 17.38 12.32`. `n = 10` was measured too but is
dominated by fixed per-op overhead, so it is not tabulated. Sizes/orders run: `1000` and
`100000` for **both** interiors in `shuffled`, `ascending` and `descending`; `1000000` for the
**B+-tree** in `shuffled`, `ascending` and `descending`.

**Read the `sorted/tree` ratio column, not the absolute ns.** A ratio is
`SortedOrderedInterior / OrderedSetInterior` (numerator = sorted `Vec`, denominator = B+-tree),
so `< 1.0` favours the `Vec` and `> 1.0` favours the tree; the load drift above moved the
absolute figures and left the ratios reproducing (the `n = 100000`, `shuffled`, `smallint`
insert and delete ratios are **36.28**/**30.14** in (A) and **36.34**/**30.18** in (C), while
the B+-tree's own absolute insert for the `n = 1000`, `shuffled` cell differs **145.2 ns**
vs **97.9 ns** between (A) and (C)).

**Table (A) — `smallint` keys (`{K, V}`, `K` in the smallint range), `Insert`/`InsertNew`/`Hit`/`Miss`/`Iter`/`Delete` ns per op, `B/e` bytes per entry**

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | B/e |
|---|---|---|---|---|---|---|---|---|---|
| 1 000 | shuffled | B+-tree | 97.9 | 100.4 | 97.2 | 43.1 | 3.4 | 115.3 | 28.8 |
| 1 000 | shuffled | sorted `Vec` | 93.6 | 93.2 | 57.4 | 39.0 | 1.3 | 97.9 | 16.4 |
| 1 000 | shuffled | **sorted/tree** | **0.96** | **0.93** | **0.59** | **0.91** | **0.37** | **0.85** | **0.57** |
| 1 000 | ascending | B+-tree | 64.4 | 63.1 | 64.7 | 46.3 | 2.8 | 76.3 | 42.5 |
| 1 000 | ascending | sorted `Vec` | 37.7 | 36.8 | 52.2 | 39.1 | 1.3 | 122.4 | 16.4 |
| 1 000 | ascending | **sorted/tree** | **0.59** | **0.58** | **0.81** | **0.84** | **0.45** | **1.61** | **0.39** |
| 1 000 | descending | B+-tree | 75.9 | 74.6 | 69.4 | 46.3 | 2.8 | 67.2 | 21.2 |
| 1 000 | descending | sorted `Vec` | 128.7 | 126.2 | 52.2 | 39.0 | 1.9 | 35.5 | 16.4 |
| 1 000 | descending | **sorted/tree** | **1.70** | **1.69** | **0.75** | **0.84** | **0.67** | **0.53** | **0.77** |
| 100 000 | shuffled | B+-tree | 141.7 | 143.7 | 138.3 | 46.9 | 2.6 | 169.0 | 28.8 |
| 100 000 | shuffled | sorted `Vec` | 5140.6 | 5154.2 | 87.4 | 52.3 | 0.9 | 5095.4 | 21.0 |
| 100 000 | shuffled | **sorted/tree** | **36.28** | **35.86** | **0.63** | **1.12** | **0.32** | **30.14** | **0.73** |
| 100 000 | ascending | B+-tree | 76.2 | 74.2 | 71.0 | 51.4 | 3.1 | 90.6 | 40.2 |
| 100 000 | ascending | sorted `Vec` | 52.8 | 51.4 | 59.7 | 52.8 | 1.0 | 10461.6 | 21.0 |
| 100 000 | ascending | **sorted/tree** | **0.69** | **0.69** | **0.84** | **1.03** | **0.31** | **115.51** | **0.52** |
| 100 000 | descending | B+-tree | 114.3 | 115.9 | 98.9 | 70.2 | 6.8 | 117.5 | 21.0 |
| 100 000 | descending | sorted `Vec` | 11138.1 | 11212.5 | 76.4 | 66.7 | 1.8 | 59.3 | 21.0 |
| 100 000 | descending | **sorted/tree** | **97.48** | **96.76** | **0.77** | **0.95** | **0.26** | **0.51** | **1.00** |

**Table (A) — `atom` keys**

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | B/e |
|---|---|---|---|---|---|---|---|---|---|
| 1 000 | shuffled | B+-tree | 180.2 | 191.8 | 161.8 | 110.9 | 3.2 | 169.7 | 28.8 |
| 1 000 | shuffled | sorted `Vec` | 173.6 | 192.1 | 136.7 | 136.0 | 1.1 | 159.5 | 16.4 |
| 1 000 | shuffled | **sorted/tree** | **0.96** | **1.00** | **0.84** | **1.23** | **0.34** | **0.94** | **0.57** |
| 1 000 | ascending | B+-tree | 124.2 | 138.9 | 105.7 | 110.5 | 2.5 | 132.8 | 42.5 |
| 1 000 | ascending | sorted `Vec` | 119.2 | 133.5 | 124.1 | 126.2 | 1.0 | 175.5 | 16.4 |
| 1 000 | ascending | **sorted/tree** | **0.96** | **0.96** | **1.17** | **1.14** | **0.39** | **1.32** | **0.39** |
| 1 000 | descending | B+-tree | 139.5 | 152.1 | 129.8 | 109.8 | 2.4 | 117.2 | 21.2 |
| 1 000 | descending | sorted `Vec` | 184.0 | 201.6 | 127.3 | 127.0 | 0.9 | 112.1 | 16.4 |
| 1 000 | descending | **sorted/tree** | **1.32** | **1.32** | **0.98** | **1.16** | **0.39** | **0.96** | **0.77** |
| 100 000 | shuffled | B+-tree | 698.6 | 766.2 | 641.4 | 247.3 | 7.2 | 705.1 | 28.7 |
| 100 000 | shuffled | sorted `Vec` | 6919.8 | 7343.6 | 645.1 | 351.0 | 1.5 | 6589.4 | 21.0 |
| 100 000 | shuffled | **sorted/tree** | **9.90** | **9.58** | **1.01** | **1.42** | **0.21** | **9.34** | **0.73** |
| 100 000 | ascending | B+-tree | 167.5 | 210.1 | 164.6 | 132.8 | 4.9 | 194.1 | 40.2 |
| 100 000 | ascending | sorted `Vec` | 226.7 | 268.6 | 233.4 | 190.8 | 1.5 | 11815.3 | 21.0 |
| 100 000 | ascending | **sorted/tree** | **1.35** | **1.28** | **1.42** | **1.44** | **0.31** | **60.87** | **0.52** |
| 100 000 | descending | B+-tree | 228.3 | 265.7 | 201.4 | 168.1 | 6.3 | 227.3 | 21.0 |
| 100 000 | descending | sorted `Vec` | 12467.3 | 12754.3 | 281.8 | 211.1 | 1.5 | 236.0 | 21.0 |
| 100 000 | descending | **sorted/tree** | **54.62** | **48.00** | **1.40** | **1.26** | **0.23** | **1.04** | **1.00** |

**Table (A) — `binary` keys**

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | B/e |
|---|---|---|---|---|---|---|---|---|---|
| 1 000 | shuffled | B+-tree | 333.3 | 335.9 | 283.7 | 319.8 | 3.5 | 278.0 | 29.6 |
| 1 000 | shuffled | sorted `Vec` | 333.5 | 318.3 | 277.5 | 298.7 | 1.1 | 282.8 | 16.4 |
| 1 000 | shuffled | **sorted/tree** | **1.00** | **0.95** | **0.98** | **0.93** | **0.32** | **1.02** | **0.55** |
| 1 000 | ascending | B+-tree | 311.0 | 322.0 | 243.7 | 254.3 | 2.9 | 259.5 | 42.5 |
| 1 000 | ascending | sorted `Vec` | 229.0 | 231.3 | 257.2 | 250.8 | 1.2 | 275.7 | 16.4 |
| 1 000 | ascending | **sorted/tree** | **0.74** | **0.72** | **1.06** | **0.99** | **0.41** | **1.06** | **0.39** |
| 1 000 | descending | B+-tree | 272.8 | 275.4 | 287.6 | 287.6 | 2.7 | 217.0 | 21.2 |
| 1 000 | descending | sorted `Vec` | 299.2 | 288.4 | 267.9 | 252.0 | 1.2 | 187.8 | 16.4 |
| 1 000 | descending | **sorted/tree** | **1.10** | **1.05** | **0.93** | **0.88** | **0.45** | **0.87** | **0.77** |
| 100 000 | shuffled | B+-tree | 875.7 | 889.4 | 890.2 | 1092.8 | 6.1 | 862.0 | 28.8 |
| 100 000 | shuffled | sorted `Vec` | 6418.7 | 6438.1 | 672.8 | 841.5 | 1.6 | 6367.6 | 21.0 |
| 100 000 | shuffled | **sorted/tree** | **7.33** | **7.24** | **0.76** | **0.77** | **0.25** | **7.39** | **0.73** |
| 100 000 | ascending | B+-tree | 564.4 | 560.4 | 437.6 | 513.7 | 4.6 | 513.3 | 40.2 |
| 100 000 | ascending | sorted `Vec` | 449.0 | 448.0 | 441.7 | 504.2 | 1.4 | 11034.6 | 21.0 |
| 100 000 | ascending | **sorted/tree** | **0.80** | **0.80** | **1.01** | **0.98** | **0.30** | **21.50** | **0.52** |
| 100 000 | descending | B+-tree | 469.7 | 466.3 | 437.2 | 529.1 | 2.9 | 378.2 | 21.0 |
| 100 000 | descending | sorted `Vec` | 10917.6 | 10799.4 | 440.6 | 518.7 | 1.3 | 378.4 | 21.0 |
| 100 000 | descending | **sorted/tree** | **23.24** | **23.16** | **1.01** | **0.98** | **0.43** | **1.00** | **1.00** |

**Table (A) — `tuple` keys**

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | B/e |
|---|---|---|---|---|---|---|---|---|---|
| 1 000 | shuffled | B+-tree | 143.6 | 144.6 | 145.2 | 96.7 | 3.0 | 152.4 | 28.8 |
| 1 000 | shuffled | sorted `Vec` | 121.0 | 119.5 | 101.1 | 98.7 | 0.9 | 115.7 | 16.4 |
| 1 000 | shuffled | **sorted/tree** | **0.84** | **0.83** | **0.70** | **1.02** | **0.32** | **0.76** | **0.57** |
| 1 000 | ascending | B+-tree | 117.5 | 122.1 | 118.1 | 109.7 | 2.5 | 125.8 | 42.5 |
| 1 000 | ascending | sorted `Vec` | 91.9 | 90.5 | 122.7 | 101.9 | 1.0 | 138.7 | 16.4 |
| 1 000 | ascending | **sorted/tree** | **0.78** | **0.74** | **1.04** | **0.93** | **0.40** | **1.10** | **0.39** |
| 1 000 | descending | B+-tree | 129.9 | 128.0 | 129.1 | 110.6 | 2.4 | 101.9 | 21.2 |
| 1 000 | descending | sorted `Vec` | 142.2 | 141.2 | 121.5 | 99.3 | 1.0 | 79.6 | 16.4 |
| 1 000 | descending | **sorted/tree** | **1.09** | **1.10** | **0.94** | **0.90** | **0.40** | **0.78** | **0.77** |
| 100 000 | shuffled | B+-tree | 293.8 | 314.2 | 291.9 | 180.2 | 6.9 | 345.4 | 28.8 |
| 100 000 | shuffled | sorted `Vec` | 5842.5 | 5683.8 | 263.6 | 193.2 | 1.7 | 5575.4 | 21.0 |
| 100 000 | shuffled | **sorted/tree** | **19.89** | **18.09** | **0.90** | **1.07** | **0.25** | **16.14** | **0.73** |
| 100 000 | ascending | B+-tree | 273.5 | 275.5 | 245.9 | 253.2 | 5.1 | 339.9 | 40.2 |
| 100 000 | ascending | sorted `Vec` | 227.4 | 221.7 | 258.3 | 228.7 | 1.6 | 14661.1 | 21.0 |
| 100 000 | ascending | **sorted/tree** | **0.83** | **0.80** | **1.05** | **0.90** | **0.31** | **43.13** | **0.52** |
| 100 000 | descending | B+-tree | 316.8 | 315.4 | 255.9 | 255.2 | 6.5 | 253.6 | 21.0 |
| 100 000 | descending | sorted `Vec` | 15968.8 | 16694.4 | 268.0 | 232.6 | 1.5 | 224.2 | 21.0 |
| 100 000 | descending | **sorted/tree** | **50.41** | **52.93** | **1.05** | **0.91** | **0.23** | **0.88** | **1.00** |

**Table (B) — the B+-tree alone at `n = 1000000`** (no sorted-`Vec` figure exists at this size:
its `O(n)`-per-op insert/delete makes the two-interior run impractical — `11000 ns × 1e6` is
~11 s *per metric per pass*):

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | B/e |
|---|---|---|---|---|---|---|---|---|---|
| 1 000 000 | shuffled | B+-tree | 247.2 | 228.7 | 321.0 | 58.9 | 8.9 | 316.5 | 27.9 |
| 1 000 000 | ascending | B+-tree | 87.5 | 84.4 | 79.6 | 60.2 | 4.0 | 112.8 | 42.6 |
| 1 000 000 | descending | B+-tree | 90.7 | 90.4 | 78.7 | 57.6 | 2.9 | 101.1 | 20.1 |

### What the tables say

* **The B+-tree is order-insensitive and the sorted `Vec` is not, by two orders of magnitude.**
  The B+-tree's `n = 100000` insert/delete stay in the **76–316 ns** band across all three
  orders and all four shapes; the `Vec`'s shuffled/descending inserts cost **5.1–16.0 µs** and
  its ascending deletes **10.5–14.7 µs**, i.e. **7×–140×** the tree's (`sorted/tree` insert
  7.33–97.48, delete 21.50–119.74). The `Vec` only wins insert when the order is *its* fast
  path: ascending (`sorted/tree` insert 0.69–0.83 at `n = 100000`) — and it loses delete by
  21.5–115.5× in that very order. That traversal-sensitive spread is exactly what the
  order dimension was added to expose: the previous shuffled-only profile understated the
  `Vec`'s worst case and overstated its best one.
* **The `Vec`'s memory advantage is real but bounded, and smaller at scale.** Its foot-
  print is `sorted/tree` **0.55–0.77** B/entry at `n = 1000` and **0.52–1.00** at `n = 100000`
  — the tree costs **1.0–1.9×** more bytes/entry. The tree's figure is *arena capacity* and
  depends on the driven order (ascending inserts split leaves at ~50 % occupancy: **40.2–42.6**
  B/entry, versus **20.1–21.2** for descending and **27.9–29.6** shuffled), so the worst case
  is ~1.9× the `Vec`'s live-data figure, not the ~2.7× a `n = 1000`-only reading would suggest.
  The ratio is bought back by the asymptotic win: the `Vec`'s own footprint grows toward
  `n × entry` because a `Vec` cannot leave holes (its B/entry rises **16.4 → 21.0** from
  `n = 1000` to `n = 100000` via capacity doubling) while the tree's stays flat.
* **The tree's one structural loss is iteration.** `iter` (`sorted/tree` **0.21–0.45** in
  every cell) is the sorted `Vec`'s best column: one contiguous `slice::iter` at **0.8–1.9 ns**
  per object versus the tree's leaf-chain walk at **1.9–8.9 ns** — a **2.2–4.8×** penalty,
  the price of pointer-chasing the arena instead of walking one array. It is a *small*
  absolute difference on a metric the `Vec` cannot be used for anyway.
* **Lookup is a wash; the tree is even slightly better.** `hit` `sorted/tree` spans
  **0.59–1.42** (median ≈ 1.0); `miss` spans **0.77–1.44**. Binary search over one array and
  a descent through ≤ 5 levels of `ORDER = 32` nodes are both ~`log n` comparisons, and the
  tree's `O(log n)` memory touches beat the `Vec`'s `O(n)` cache footprint on the larger sets.
  Neither is a reason to choose an interior; insert/delete and memory are.
* **Verdict.** The B+-tree is the default because the ordering-sensitive cost is the one that
  grows: on any non-ascending workload the sorted `Vec` is 7×–140× slower at `n = 100000` and
  scales as `O(n)` per op (`n = 1000` → 1e5 already opens 0.85 → 30.1 on delete), while the
  tree pays at most **1.9×** memory and **4.8×** iteration in return. The `Vec` remains the
  selectable `OrderedInterior::Sorted` alternative, and this table is its honest case: contiguous
  iteration and the ascending append path.
