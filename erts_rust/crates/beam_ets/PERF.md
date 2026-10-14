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
store **plus the index's capacity**, while the oracle's figure tracks live data on a small
fixed base — so the absolute values are not comparable (loaded 100 keys: crate
`set/chained` 3468 words vs oracle `set` 1606 words). What is comparable, and what the test
asserts, is the **contract**. Oracle figures (`escript`, `/proc/loadavg` `2.93 4.63 5.79` for
the delete family and the progression, `2.60 4.12 5.50` for the last run):

| family | oracle | crate |
|---|---|---|
| `delete/2`, `delete_object/2`, `delete_all_objects/1` each return a 100-object table to the **empty** floor | `set`/`bag`/`duplicate_bag` 306 → 1606 → **306**; `ordered_set` 140 → 1540 → **140**; `size` → 0 | every shape: 3 cycles land on one stable floor, strictly below the loaded figure and never below the empty table's; `delete_all_objects` and (after the fix below) a per-key emptying return **exactly** to the empty figure |
| overwrite of an existing key (`m3` vs `m2`) | `set`/`ordered_set` **flat** (`m3 == m2`); `bag`/`duplicate_bag` grow by one object | same classification, exactly |
| `delete_object/2` of the overwritten object (`m4`) | returns exactly to the pre-overwrite figure (`m1` on the upsert kinds, `m2` on the multi kinds) | direction verified (`m4 < m3`, `m4 >= m0`); on a chained index the *bucket array capacity* grown by the second key is retained until the table empties, so `m4` can sit above `m1` (measured 71 words vs `m1` = 55 at `/proc/loadavg` `2.70 4.01 5.43`) |
| the removal path's endpoint (`m6` vs `m0`) | exactly `m0` (`m6_eq_m0` true for every kind) | `<= m0`, and `== m0` once the interior is released (see below) |
| heir gift data | `{heir, self(), {a_gift, [1..8]}}` costs **+23** words; `{heir, self(), undefined}` costs **+0**; 2000 set/clear rounds land on the cleared figure | +19 words for an equally shaped gift; arity-2 and `ClearHeir` return exactly to the base figure; 2000 rounds flat |
| `give_away/3` | owner moves, `memory` **unchanged**, `size` unchanged | same |
| heir transfer on owner death | table survives, owner = heir, heir **kept installed**, `memory` does not fall | same (`memory` unchanged, gift copy retained, reclaimed exactly on `ClearHeir`) |

### Fix made: an emptied interior now releases its index storage

The oracle keeps **no** index storage for an empty table (`delete/2` and
`delete_object/2` put `ets:info(T, memory)` back on the empty figure), but the crate's
`delete`/`delete_object` left the interior's capacity in place: after deleting every key of
a 100-object table the crate reported 2368 words (chained) / 135 words (CHAMP) / 184 words
(B+-tree) against its own empty figure of 24 / 7 / 24. `Table::delete`/`delete_object` now
route through a new private `Table::release_empty_interior`, which rebuilds the interior once
the last object has left it — the same `build_interior` operation `delete_all_objects` already
used, so the two removal paths now converge and the figure returns to the empty-table floor.
Only a removal that actually happened calls it, so `delete` of an absent key never churns the
index.

The residual, still-documented difference is **capacity vs live data**: the crate's
`memory()` counts the index's allocated capacity, so an index that has shrunk to *one* object
still reports the bucket array/nodes it grew (the `m4` row above). Making the hash interiors
release capacity per bucket (ERTS's "free an empty bucket") and the ordered arena shrink on
delete is the natural follow-up; it is a throughput/accounting refinement, not a leak —
`tests/ets_reclamation.rs` pins that every removal path's floor is stable across churn.
