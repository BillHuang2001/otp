# `ordered_interior` — the `ordered_set` interiors

## Intent
The two selectable `ordered_set` indexes behind the crate-private `Interior` seam:
`OrderedSetInterior` (a K-ary B+-tree `Tree`, the crate default) and `SortedOrderedInterior`
(a sorted `Vec`, the cross-check/measurement alternative). Owns the *observable* ordered-set
semantics only: key equality/ordering for `lookup`/`member`/`delete` and ordered iteration are
the arithmetic key order `beam_term::compare::cmp_arith`, so `1` and `1.0` are the same key;
`delete_object/2` alone compares the whole object exactly with `compare::eq`.
Term order is never re-implemented here (workspace Prevent-Duplication rule).

## API Surface
- `OrderedSetInterior::{new, first, last, first_key, last_key, next, prev, next_from, prev_from, memory}`
  + its `Interior` impl. `next_from(bound, inclusive)`/`prev_from(bound, inclusive)` are the range
  API; `bound` need not be a stored key.
- `SortedOrderedInterior::{new, len, is_empty, memory, find, first, last, first_key, last_key, next,
  prev, next_from, prev_from, insert, insert_new, delete, delete_object}` + its `Interior` impl — the
  same observable rules over one sorted `Vec`.
- Both are re-exported at the crate root and chosen by `kind::OrderedInterior::{BPlusTree,Sorted}`
  through `table::build_interior`. Not the cross-crate seam: dependents go through `crate::Table`.

## Invariants (asserted by `Tree::check_invariants`, test-only)
1. A branch holds `keys.len() == children.len()`, separators strictly ascending in `cmp_arith`.
2. `separator[i] == max(children[i])` — the *exact* subtree maximum, not a lower bound.
3. Every node holds **at most `ORDER=32`** entries/children (the fan-out that bounds node size and
   makes a split amortized `O(1)` — one split per `ORDER/2` inserts), and every node below the root
   holds within `[MIN_OCCUPANCY=16, ORDER=32]`; the root leaf may be empty, a root branch keeps `>= 2`
   children. Both arms assert the upper bound explicitly, so an unbounded-node-size regression fails
   `check_invariants` loudly.
4. All leaves are at the same depth. 5. The leaf chain visits every leaf exactly once, left to right.

## Design Decisions
- **Separators are always re-materialised from the rightmost live leaf** via
  `NodeStore::subtree_max_key`, never read back from `branch.keys` to construct one: an `ets:insert`
  upsert replaces the object a stored separator aliased and the caller reclaims that object's storage
  right after. `delete_rec` re-derives the settled child's separator from the surviving leaf at every
  level of the unwind; `insert_rec` instead propagates the new subtree maximum up in its `InsertStep`
  return value (so an ordinary insert re-derives nothing) and re-materialises a separator only where
  the change actually moves it — on a split (`split_overfull`/`split_root`/`adopt_split`) or when the
  descended-into subtree is the last child. `child_index` is the only production reader of
  `branch.keys`.
- `child_index` **binary-searches** the strictly-ascending separators (a hand-rolled lower bound: the
  first separator `>= key`, clamped to the last child) — the tree's hottest helper.
- Split at `ORDER+1`, cut at `ORDER/2` (16/17); rebalance borrows from a sibling with `> MIN`
  occupancy or merges (siblings then sum to `<= ORDER`), so occupancy stays bounded and the height
  changes only at the root, which collapses when it drops to a single child.
- A same-`cmp_arith`-key insert replaces **both** the object and the stored key term (the oracle's
  key-follows-object rule; probed against OTP 29).
- **The evidence suite is tuned to hit the split/merge boundary, and says so.** The randomised trace
  spends 34 of its 60 op slots on `insert` precisely so the live-key count crosses `ORDER = 32` (it
  reaches 68), and it asserts that peak, so diluting the insert share fails the test instead of
  quietly reducing the trace to a small-table walk. The benchmark likewise drives each interior in
  three key orders rather than one, because a single order measures the sorted `Vec`'s best case
  (ascending inserts) or its worst case (ascending deletes) and never both — the two interiors'
  crossover is only visible when both are in the same run.

## Complexity (as implemented)
Per-operation node visits, `O(K)` work per node (`Vec::insert`/`remove` in a `K`-sized node) and
`O(log n)` term comparisons overall (`O(log ORDER)` per node, from the two binary searches):
- `find`, `first`, `last`, `next`, `prev` — `O(log n)`: one descent, plus for `next`/`prev` at most one
  extra descent to the neighbouring subtree's leftmost/rightmost leaf (`next_subtree_key`).
- `insert` — `O(log n)` **amortized**. The `InsertStep` mechanism carries the new subtree maximum up,
  so only a split pays a `subtree_max_key` descent (a comparison-free pointer chase to a rightmost
  leaf), and splits are amortized `O(1)` per insert (one per `ORDER/2` inserts). A split cascading up
  every level makes a single insert worst-case `O(log^2 n)`.
- `delete` — `O(log^2 n)` node visits, unconditionally: the unwind re-derives the settled child's
  maximum with a `subtree_max_key` descent at *every* level instead of carrying it up, so the tree
  depth is paid once per level. Those re-derivations are pointer chases (no term comparisons), so the
  comparison count stays `O(log n)`. Correctness-driven, not a bug: the maximum must alias a *live*
  table-owned term, which is why the term is re-materialised rather than propagated blindly.
- Ordered iteration is `O(n)` (a walk of the leaf chain).

The selectable sorted `Vec` (`sorted.rs`) is `O(log n)` lookup and `O(n)` insert/delete — that is
exactly why the B+-tree is the default (see `../interior_bench.rs`'s `ordered_interior_microbenchmark`).

## Constraints
- Every key comparison goes through `beam_term::compare::cmp_arith`; no `unsafe` in this directory.
- `expect`/`unreachable!` sit only on internal invariants (a `NodeId`/index the tree itself derived)
  and are unreachable from the public API; panics are never control flow for runtime data.
- The B+-tree assumes `cmp_arith` is a strict total order (its only caveat is the documented
  `beam_term` best-effort `f64` comparison for out-of-range bignum<->float keys).
- Files target ~1000 lines; the big test module is the `tests` submodule in `tests.rs`.

## Test Strategy
`check_invariants` walks the whole tree against all five invariants (incl.
`separator[i] == max(child[i])` **and** the `<= ORDER` fan-out bound) after every step of scattered
200-key and 10 000-key insert/delete/re-split churn, and drain-to-empty; an oracle `Vec` cross-checks
`size` and the ascending key run. A **100 000-key** churn case
(`one_hundred_thousand_keys_survive_churn_with_the_invariants_held`) scatters inserts (LCG shuffle),
periodically re-asserts `check_invariants`, checks membership/`lookup` and
`first`/`last`/`next`/`prev` (incl. bounds), walks the full ascending key run, removes every third key,
re-checks membership + the walk, re-inserts everything, walks again, then drains back to empty — all
with immediates (no `Heap`), so it measures the index alone (~0.4 s). `tests.rs` tests cover the empty
case, replace/`insert_new`/`delete_object`, mixed key classes, present/absent/boundary navigation, the
replace-key-term rule, and memory accounting across churn; both interiors are driven through the `Interior` seam
by shared helpers. `../interior_parity.rs` proves the B+-tree and the sorted `Vec` produce identical
observable traces. End-to-end `ordered_set` behaviour is oracle-differential-tested in
`../tests/differential/` (navigation, 10 001-key, matchspec, ...). No `#[ignore]` in `tests.rs` or
`interior_parity.rs`, and no self-comparison tautologies.

`../tests/ets_ordered_oracle.rs` is the **randomised** differential trace against the live oracle —
the one `#[ignore]`d test here (it needs the system Erlang and a few seconds of wall clock; run it
with `--ignored --nocapture`, and it **skips loudly** rather than passing vacuously when no oracle is
found). A 73-term mixed-type key pool is driven by a SplitMix64-finalised LCG through a **60-slot** op
space: 34 `insert` slots, one slot each for `insert_new`, `delete`, `delete_object`, `take`,
`delete_all_objects`, `lookup`, `member`, `first`, `last`, `next`, `prev`, `select`, `select_count`,
`match_object`, `select_reverse`, `foldl`, `update_counter`, `update_element`, `foldr`,
`lookup_element/3` and `lookup_element/4`, plus a **`next`/`prev` walk** dimension (a full `first`+`next`
/ `last`+`prev` chain, and a chain starting from the `aux` pool key — usually an absent key, i.e. a
range bound) and a **key-boundary** slot `insert_block` (a fresh ascending contiguous integer block
longer than `ORDER = 32`, forcing a split every time it fires). Every key the table yields renders as
its pool index, and `{size, in-order tab2list, op result}` is asserted against the oracle after
**every** step; the counter/element ops target element position 2 (an object is `{K, V}`), the slot both
sides agree on. 3 seeds × 600 steps = **1800 compared steps**, zero divergence, peak **68** live keys,
longest single walk **67** keys — the trace is required to cross `ORDER = 32` and to record a walk
longer than `ORDER` (the test asserts its own peak and that every new slot fired) so a future op-mix
change cannot silently stop exercising node splits, the merge-on-delete churn, or a `next`/`prev` walk
across a split boundary.

A deterministic, always-on counterpart lives in `../tests/ets_fixtable.rs`: a `first`+`next` /
`last`+`prev` chain over 100 keys (strictly > 2 × `ORDER`) interleaved with inserts/deletes — including
each visited anchor's own deletion under fixation — asserting every key is visited exactly once in
order, plus an absent-key anchor walked across the split boundary in both directions. No `#[ignore]`.

`../interior_bench.rs::ordered_interior_microbenchmark` is the reproducible B+-tree-vs-sorted-`Vec`
comparison (it asserts every measurement, so a wrong-but-fast index fails the run):
```text
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets \
    --lib ordered_interior_microbenchmark -- --nocapture
```
Use `BEAM_ETS_BENCH_SIZES=1000,100000 BEAM_ETS_BENCH_REPEATS=5` for stable figures (each cell is the
per-metric **minimum** over the passes) and `BEAM_ETS_BENCH_INDEX=bplus|sorted` to run one interior.
`BEAM_ETS_BENCH_ORDER=shuffled|ascending|descending|all` adds the **key-order** dimension (default
`shuffled`, the single-order behaviour earlier runs used): the order is irrelevant to the tree and
decisive for the sorted `Vec`, whose ascending inserts are its append fast path while its ascending
deletes are the whole-tail-shift worst case — so both interiors' best *and* worst cases get measured.
Absolute ns are meaningless on a shared host — read the printed `sorted/tree` ratio column. On such a
run the B+-tree wins exactly where a `Vec`'s tail shift bites: at n = 1e5 the sorted `Vec` is 7.3–97.5×
slower inserting and 21.5–119.7× slower deleting across the four key shapes and three orders, while
it stays ahead only on the comparison-light, traversal-shaped work — 0.21–0.45× the iteration time
(one contiguous array walk versus the leaf chain) — and is a wash on hit/miss lookup (0.59–1.42× /
0.77–1.44×). Its index footprint is the smaller one: **16.4–21.0** vs the tree's **20.1–42.6**
bytes/entry, though the tree's figure is *arena capacity* and varies with the driven order (ascending
inserts split leaves at ~50 % occupancy). The full measured table, the sizes/orders run and each
run's `/proc/loadavg` are in `../../PERF.md` §3.

## Known Issues / Notes
- `next_from`/`prev_from` with `inclusive = true` has no production consumer yet: only `Tree::next`/
  `prev` call them (`inclusive = false`). It is the range API a later `ets:select`/`ets:next`-style
  scan wires up (documented in `mod.rs`).

## Routing Table
- `mod.rs` → `OrderedSetInterior` (B+-tree facade + `Interior` impl); declares `mod tests;` → `tests.rs` (the crate's biggest test module)
- `tree.rs` → `Tree`: insert/split, delete/borrow-or-merge, navigation, `check_invariants`
- `node.rs` → `Node`/`LeafNode`/`BranchNode`/`Entry`, the `NodeStore` arena, `search_entries`, `child_index`, `ORDER`/`MIN_OCCUPANCY`
- `iter.rs` → `LeafIter` (the leaf-chain ordered iterator)
- `sorted.rs` → `SortedOrderedInterior` (the sorted-`Vec` alternative) + its tests

## See Also
- `../../CONTEXT.md` (`beam_ets`) → the `Interior` seam, the crate API surface and the `ordered_set` interior entry.
- `../table/interior.rs` → the `Interior` trait both interiors implement.
- `../../../../erts/emulator/beam/erl_db_tree.c` → the legacy CATREE `ordered_set` being mirrored (read-only).
