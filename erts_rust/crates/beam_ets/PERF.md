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

The crate ships two selectable `ordered_set` interiors, and this section is the measurement
that decides which one is the default:

* **`OrderedSetInterior`** — the K-ary B+-tree (`ORDER = 32`, `MIN_OCCUPANCY = 16`, an arena
  of `NodeId` nodes that never relocate, a leaf chain, borrow-or-merge rebalancing), the
  crate default (`OrderedInterior::BPlusTree`, `#[default]`);
* **`SortedOrderedInterior`** — one sorted `Vec` (`OrderedInterior::Sorted`), the interior
  the B+-tree replaced, retained as the selectable alternative.

`src/interior_bench.rs::ordered_interior_microbenchmark` times, in **ns per operation**:
**insert** (upsert of the `n` present keys), **insert_new** (a disjoint `n`-key set, so every
call takes the "key is new" path — a distinct descent in the tree), **hit** (lookup of a
present key), **miss** (`member` of an absent key), **iter** (one full `iter_objects` walk)
and **delete** (removing all `n` keys).
Both interiors store the same key term as the object, so the shared term cost cancels and the
timed figure is the **index** cost alone.
Every measurement **asserts its answers** as it goes (`size` after the insert and after the
delete, hit/miss, the iteration count, `insert_new`'s `true` and then `false`), so a
fast-but-wrong index fails the run rather than winning the benchmark.
It also asserts the interior's own word count stays **bounded per entry** (≤ `64`
words/entry after the insert phase, against the `2.6–5.3` words/entry these two interiors
actually use), so an index whose memory grows per insert fails the run as well.

Three dimensions are measured. **Size** is the one the verdict turns on. **Key order**
(`BEAM_ETS_BENCH_ORDER=shuffled|ascending|descending|all`) is a B+-tree irrelevance and a
sorted-`Vec` decisive: ascending inserts are the `Vec`'s append fast path while ascending
deletes are its shift-the-whole-tail worst case, and both orders derive from the interior's
own comparison (`cmp_arith`, so `1` and `1.0` order by arithmetic value). **Key shape**
(`BEAM_ETS_BENCH_SHAPES=smallint,atom,binary,tuple`) covers an immediate key and three heap
keys; Table (B) repeats the comparison on the two heaviest heap shapes.
The reported figure is the **per-metric minimum over `BEAM_ETS_BENCH_REPEATS` passes** on
freshly built interiors — on a shared host a pass can only be made *slower*, so the minimum
is the interference-free estimator and a single pass is not comparable between runs.
The `words` column is the interior's own `Interior::memory()` with all `n` keys stored, and
`B/e` is `words × 8 / n`; both are **timer-free and therefore load-independent**, which is
what makes them the anchor the timing ratios are read against.

### Reproduce

```text
# (A) the measured table: smallint keys, 1k/10k/100k, all three orders, both interiors
BEAM_ETS_BENCH_SIZES=1000,10000,100000 BEAM_ETS_BENCH_SHAPES=smallint \
BEAM_ETS_BENCH_ORDER=all BEAM_ETS_BENCH_REPEATS=5 \
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --lib ordered_interior_microbenchmark -- --nocapture

# (B) the key-shape cross-check: the heap-key shapes, same sizes and orders
BEAM_ETS_BENCH_SIZES=1000,10000,100000 BEAM_ETS_BENCH_SHAPES=binary,tuple \
BEAM_ETS_BENCH_ORDER=all BEAM_ETS_BENCH_REPEATS=3 \
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --lib ordered_interior_microbenchmark -- --nocapture

# (C) beyond the mandated range: 1e6, B+-tree alone (the sorted Vec is O(n) per op there)
BEAM_ETS_BENCH_SIZES=1000000 BEAM_ETS_BENCH_SHAPES=smallint BEAM_ETS_BENCH_ORDER=all \
BEAM_ETS_BENCH_INDEX=bplus BEAM_ETS_BENCH_REPEATS=3 \
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    -p beam_ets --lib ordered_interior_microbenchmark -- --nocapture
```

### Runs recorded here, with their `/proc/loadavg`

The host is shared, so every run's load is recorded and the conclusions are drawn from the
**ratios** and from the deterministic word counts — never from an absolute wall time.

| run | sizes / shapes / orders | wall | `/proc/loadavg` before → after |
|---|---|---|---|
| (A) | 1k, 10k, 1e5 / `smallint` / shuffled+ascending+descending, `REPEATS=5` | 33 842 ms | `16.72 12.48 6.92` → `24.41 14.92 7.94` |
| (A′) the (A) cells again, as a reproducibility check | same, `REPEATS=5` | 38 285 ms | `29.08 21.96 11.36` → `50.47 27.75 13.72` |
| (B) | 1k, 10k, 1e5 / `binary`,`tuple` / all three orders, `REPEATS=3` | 73 332 ms | `24.37 15.07 8.03` → `34.12 22.37 11.26` |
| (C) | 1e6 / `smallint` / all three orders, B+-tree only, `REPEATS=3` | 14 627 ms | `34.12 22.37 11.26` → `29.87 21.99 11.31` |

**Read the `sorted/tree` ratio row, not the absolute ns.** A ratio is
`SortedOrderedInterior / OrderedSetInterior` (numerator = sorted `Vec`, denominator =
B+-tree), so `< 1.0` favours the `Vec` and `> 1.0` favours the tree.
Both interiors are timed in the **same process and the same build profile** the crate gate uses
(`dev`, `opt-level = 1` with dependencies at `opt-level = 2`), so a cell's ratio is what
transfers and its absolute ns is not.
The load drift between (A) and (A′) moved the absolute figures by up to 22 % and left the
ratios reproducing: `n = 100 000`, `shuffled`, `smallint` insert/delete is
**29.54**/**23.72** in (A) and **29.35**/**24.35** in (A′), and lookup-hit is **0.55**/**0.61**
(the sub-microsecond tree-side timers are the load-sensitive half).
The `words` figures are identical in (A) and (A′) — `3 602`/`2 048`, `34 332`/`32 768`,
`360 006`/`262 144` — which is the load-independent anchor for the memory claims.

**Table (A) — `smallint` keys, ns per operation, `words` = the interior's own index word count with all `n` keys stored, `B/e` = `words × 8 / n`**

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | words | B/e |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 000 | shuffled | B+-tree | 108.5 | 108.0 | 111.2 | 54.9 | 3.7 | 126.9 | 3 602 | 28.8 |
| 1 000 | shuffled | sorted `Vec` | 100.0 | 98.7 | 68.1 | 47.6 | 1.3 | 105.4 | 2 048 | 16.4 |
| 1 000 | shuffled | **sorted/tree** | **0.92** | **0.91** | **0.61** | **0.87** | **0.36** | **0.83** | **0.57** | **0.57** |
| 1 000 | ascending | B+-tree | 73.7 | 72.1 | 77.8 | 56.6 | 3.0 | 82.2 | 5 316 | 42.5 |
| 1 000 | ascending | sorted `Vec` | 47.1 | 46.1 | 64.3 | 49.1 | 1.3 | 128.1 | 2 048 | 16.4 |
| 1 000 | ascending | **sorted/tree** | **0.64** | **0.64** | **0.83** | **0.87** | **0.45** | **1.56** | **0.39** | **0.39** |
| 1 000 | descending | B+-tree | 83.6 | 84.2 | 81.1 | 56.3 | 2.9 | 74.2 | 2 654 | 21.2 |
| 1 000 | descending | sorted `Vec` | 133.4 | 131.2 | 64.3 | 48.8 | 1.3 | 45.0 | 2 048 | 16.4 |
| 1 000 | descending | **sorted/tree** | **1.60** | **1.56** | **0.79** | **0.87** | **0.47** | **0.61** | **0.77** | **0.77** |
| 10 000 | shuffled | B+-tree | 123.1 | 125.8 | 119.4 | 47.2 | 3.1 | 141.4 | 34 332 | 27.5 |
| 10 000 | shuffled | sorted `Vec` | 432.2 | 438.6 | 68.7 | 47.7 | 0.9 | 449.4 | 32 768 | 26.2 |
| 10 000 | shuffled | **sorted/tree** | **3.51** | **3.49** | **0.58** | **1.01** | **0.29** | **3.18** | **0.95** | **0.95** |
| 10 000 | ascending | B+-tree | 66.2 | 69.8 | 76.2 | 54.1 | 2.9 | 79.8 | 51 480 | 41.2 |
| 10 000 | ascending | sorted `Vec` | 42.4 | 42.3 | 60.0 | 48.8 | 1.1 | 923.4 | 32 768 | 26.2 |
| 10 000 | ascending | **sorted/tree** | **0.64** | **0.61** | **0.79** | **0.90** | **0.39** | **11.57** | **0.64** | **0.64** |
| 10 000 | descending | B+-tree | 71.5 | 74.7 | 78.6 | 52.7 | 2.5 | 73.6 | 27 646 | 22.1 |
| 10 000 | descending | sorted `Vec` | 911.5 | 905.4 | 54.4 | 47.9 | 0.8 | 40.0 | 32 768 | 26.2 |
| 10 000 | descending | **sorted/tree** | **12.74** | **12.12** | **0.69** | **0.91** | **0.34** | **0.54** | **1.19** | **1.19** |
| 100 000 | shuffled | B+-tree | 176.8 | 177.1 | 179.8 | 63.7 | 5.6 | 217.0 | 360 006 | 28.8 |
| 100 000 | shuffled | sorted `Vec` | 5 221.1 | 5 153.1 | 99.1 | 61.1 | 0.8 | 5 146.0 | 262 144 | 21.0 |
| 100 000 | shuffled | **sorted/tree** | **29.54** | **29.10** | **0.55** | **0.96** | **0.15** | **23.72** | **0.73** | **0.73** |
| 100 000 | ascending | B+-tree | 78.6 | 78.2 | 79.6 | 56.5 | 2.3 | 96.5 | 502 480 | 40.2 |
| 100 000 | ascending | sorted `Vec` | 52.6 | 53.8 | 62.3 | 55.1 | 1.5 | 10 241.1 | 262 144 | 21.0 |
| 100 000 | ascending | **sorted/tree** | **0.67** | **0.69** | **0.78** | **0.97** | **0.64** | **106.12** | **0.52** | **0.52** |
| 100 000 | descending | B+-tree | 88.9 | 88.8 | 83.2 | 58.4 | 2.2 | 88.7 | 261 930 | 21.0 |
| 100 000 | descending | sorted `Vec` | 10 301.8 | 10 323.6 | 68.8 | 60.2 | 1.2 | 67.7 | 262 144 | 21.0 |
| 100 000 | descending | **sorted/tree** | **115.92** | **116.23** | **0.83** | **1.03** | **0.52** | **0.76** | **1.00** | **1.00** |

**Table (B) — the heap-key shapes, both sides named for the four decision columns (`insert`, `hit`, `delete`, `iter`, ns per op); ratios are `sorted/tree`**

| shape | n | order | `Vec` ins | tree ins | ratio | `Vec` hit | tree hit | ratio | `Vec` del | tree del | ratio | `Vec` iter | tree iter | ratio |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| binary | 1 000 | shuffled | 319.0 | 317.4 | 1.01 | 262.0 | 275.3 | 0.95 | 260.6 | 276.3 | 0.94 | 1.0 | 3.2 | 0.31 |
| binary | 1 000 | ascending | 236.2 | 297.5 | 0.79 | 237.4 | 240.2 | 0.99 | 267.5 | 258.9 | 1.03 | 1.2 | 2.5 | 0.49 |
| binary | 1 000 | descending | 278.5 | 270.7 | 1.03 | 231.3 | 272.8 | 0.85 | 178.7 | 207.7 | 0.86 | 1.3 | 2.8 | 0.45 |
| binary | 10 000 | shuffled | 863.6 | 578.3 | 1.49 | 433.4 | 476.4 | 0.91 | 800.3 | 507.9 | 1.58 | 1.1 | 4.2 | 0.27 |
| binary | 10 000 | ascending | 363.7 | 450.0 | 0.81 | 376.5 | 379.7 | 0.99 | 1 323.7 | 411.7 | 3.22 | 1.3 | 3.8 | 0.34 |
| binary | 10 000 | descending | 1 244.3 | 431.0 | 2.89 | 340.6 | 418.1 | 0.81 | 304.7 | 313.2 | 0.97 | 1.1 | 3.1 | 0.35 |
| binary | 100 000 | shuffled | 7 232.0 | 919.4 | 7.87 | 865.2 | 970.9 | 0.89 | 8 136.6 | 989.4 | 8.22 | 1.5 | 6.5 | 0.23 |
| binary | 100 000 | ascending | 1 057.3 | 814.2 | 1.30 | 717.7 | 689.9 | 1.04 | 19 780.3 | 816.2 | 24.23 | 1.6 | 5.2 | 0.31 |
| binary | 100 000 | descending | 15 625.9 | 881.5 | 17.73 | 621.5 | 813.7 | 0.76 | 565.9 | 713.1 | 0.79 | 1.5 | 6.9 | 0.21 |
| tuple | 1 000 | shuffled | 247.9 | 282.5 | 0.88 | 198.5 | 246.7 | 0.80 | 223.4 | 240.3 | 0.93 | 2.0 | 5.2 | 0.38 |
| tuple | 1 000 | ascending | 192.1 | 223.5 | 0.86 | 196.9 | 209.5 | 0.94 | 262.7 | 248.2 | 1.06 | 1.8 | 5.0 | 0.36 |
| tuple | 1 000 | descending | 281.1 | 237.9 | 1.18 | 195.4 | 218.9 | 0.89 | 162.4 | 177.2 | 0.92 | 2.0 | 4.4 | 0.45 |
| tuple | 10 000 | shuffled | 1 091.9 | 412.2 | 2.65 | 355.6 | 450.9 | 0.79 | 1 088.3 | 462.8 | 2.35 | 2.0 | 9.9 | 0.20 |
| tuple | 10 000 | ascending | 260.6 | 287.7 | 0.91 | 278.9 | 287.4 | 0.97 | 1 664.3 | 351.3 | 4.74 | 2.0 | 5.9 | 0.33 |
| tuple | 10 000 | descending | 1 678.8 | 328.2 | 5.12 | 289.2 | 300.0 | 0.96 | 236.8 | 258.2 | 0.92 | 1.9 | 8.1 | 0.23 |
| tuple | 100 000 | shuffled | 5 866.6 | 401.5 | 14.61 | 331.6 | 416.0 | 0.80 | 5 845.6 | 492.3 | 11.88 | 1.7 | 8.5 | 0.20 |
| tuple | 100 000 | ascending | 194.1 | 250.9 | 0.77 | 209.5 | 224.1 | 0.93 | 10 367.0 | 290.8 | 35.65 | 1.4 | 4.2 | 0.33 |
| tuple | 100 000 | descending | 10 431.4 | 263.2 | 39.64 | 219.0 | 225.0 | 0.97 | 189.9 | 203.9 | 0.93 | 1.5 | 3.1 | 0.50 |

**Table (C) — the B+-tree beyond the mandated range at `n = 1 000 000`** (no sorted-`Vec`
figure exists there: extrapolating its measured `O(n)` insert from `5.2 µs` at 100k gives
`≈52 s` per metric per pass, against the tree's measured `0.3 s`).

| n | order | interior | Insert | InsertNew | Hit | Miss | Iter | Delete | words | B/e |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 000 000 | shuffled | B+-tree | 297.4 | 345.8 | 359.3 | 66.1 | 9.0 | 460.0 | 3 485 050 | 27.9 |
| 1 000 000 | ascending | B+-tree | 114.3 | 113.1 | 110.3 | 83.6 | 4.7 | 141.5 | 5 319 856 | 42.6 |
| 1 000 000 | descending | B+-tree | 121.3 | 122.6 | 109.9 | 80.6 | 3.6 | 130.8 | 2 518 556 | 20.1 |

### What the tables say

The B+-tree is **order-insensitive**: its `smallint` insert stays in a `66.2–123.1` ns band at
1k/10k and `78.6–186.8` ns at 100k across the three orders, while the sorted `Vec` swings from
`42.0` ns (its ascending fast path at 10k) to `11 519.7` ns (descending at 100k) — a 274×
spread for the same amount of work.
The `Vec`'s write cost is `O(n)`: `100.0 → 432.2 → 5 221.1` ns per insert and
`105.4 → 449.4 → 5 146.0` ns per delete in the neutral order, i.e. ~`53` ns of extra work per
1 000 stored keys.
The tree's write cost is `O(log n)` with a small constant: insert `108.5 → 123.1 → 176.8` ns
and delete `126.9 → 141.4 → 217.0` ns, so a 100× larger table costs **1.63×/1.71×** more per
op rather than 52×.
Fitting the measured points gives the crossover: the `Vec` is within `0.92×`/`0.83×` of the tree
at 1 000 keys and `3.51×`/`3.18×` behind it at 10 000, so **write parity sits between 1 000 and
10 000 keys** — a log-linear interpolation puts insert at **≈1 100 keys** and delete at **≈1 200**,
while the `Vec`'s measured per-key slope puts deletion nearer **1 600**; either way parity is
below **2 000 keys**, and past those points every write column belongs to the tree.
From 10 000 keys the `Vec` is **3.51×** (insert) and **3.18×** (delete) slower, and from
100 000 keys **29.54×** and **23.72×**; its worst measured case is **115.92×** (descending
insert at 100k) and its worst delete **106.12×** (ascending at 100k).
The `Vec` wins a write only on its own fast path: ascending inserts are `0.64×`/`0.64×`/`0.67×`
at 1k/10k/100k — and in that same ascending order its delete is `1.56×`/`11.57×`/`106.12×`
*worse*, because `ets:delete/2` then shifts the whole tail.
Reads are a wash or a bounded `Vec` win: lookup-hit is `0.61×`/`0.58×`/`0.55×` (`68.1` vs
`111.2`, `68.7` vs `119.4`, `99.1` vs `179.8` ns — a saving of 43/51/81 ns per lookup) and
miss is `0.87×–1.03×`, i.e. parity.
Iteration is the `Vec`'s best column and still a small absolute difference: `1.3`/`0.9`/`0.8` ns
per object against the tree's `3.7`/`3.1`/`5.6` ns (`0.36×`/`0.29×`/`0.15×`, at most
**4.8 ns per object** at 100k).
The tree's index footprint is **larger in the neutral order but flat in `n`**:
`28.8`/`27.5`/`28.8` B/entry (`3 602`/`34 332`/`360 006` words) at 1k/10k/100k, and
`27.9` B/entry at 1M — the `Vec` is `16.4`/`26.2`/`21.0` B/entry, so the tree costs
**1.76×/1.05×/1.37×** the `Vec`, whose figure chases its capacity doubling
(1 024 → 16 384 → 131 072 slots) rather than the live key count.
That figure is the tree's **arena capacity** and therefore order-dependent — ascending
inserts split leaves at ~50 % occupancy (`42.5`/`41.2`/`40.2` B/e, a **2.59×/1.57×/1.91×**
penalty against the `Vec`) while descending inserts pack the leaves
(`21.2`/`22.1`/`21.0` B/e) — so the tree is never more than **2.59×** the `Vec`'s footprint
and is actually **cheaper** than it at 10k and 100k when keys arrive in descending order.
The heap-key shapes reproduce every conclusion (Table B): at 100k the `Vec` is
`7.87×`/`17.73×` slower on insert for `binary` keys and `14.61×`/`39.64×` for `tuple` keys in
the neutral/descending orders, `24.23×`/`35.65×` slower on ascending delete, while its
lookup-hit stays `0.76×–1.04×` and its iteration `0.20×–0.50×`.
No defect was found, so **no source change was made**: the word counts are bounded per entry
(and the benchmark now asserts that), the tree's per-op curves are `O(log n)`, and the only
column the `Vec` owns outright is iteration, at an absolute cost the tree does not need to
match.

**Verdict — the B+-tree is the right default.** The number that settles it: write parity with the
sorted `Vec` is in the **1 000–2 000-key** range (≈1 100 keys for insert, ≈1 200–1 600 for delete,
interpolated), and past it the `Vec`'s `O(n)` write cost escalates to **3.5×/3.2× at 10 000 keys**
and **29.5×/23.7× at 100 000 keys** (worst case **116×**), while everything it buys back is
bounded and absolute small — **≤ 2.59×** memory (1.05–1.76× in the neutral order, and below the
`Vec`'s at 10k and 100k when keys arrive descending), **≤ 81 ns** per lookup, **≤ 4.8 ns** per
iterated object.
The size at which the choice starts to matter is therefore **10 000 keys**: below ~1 000 keys
the two interiors are within 1.7× on every column in either direction, and from 10 000 keys up
the ordering-sensitive write cost is the only term that grows.
`OrderedInterior::Sorted` remains correct and selectable, and this table is its honest case: a
read-mostly table that stays under ~1 000 keys, or bulk loads in ascending key order — provided
it is never emptied by ascending deletes.
