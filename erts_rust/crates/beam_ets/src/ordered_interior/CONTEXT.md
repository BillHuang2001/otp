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
replace-key-term rule, and memory monotonicity; both interiors are driven through the `Interior` seam
by shared helpers. `../interior_parity.rs` proves the B+-tree and the sorted `Vec` produce identical
observable traces. End-to-end `ordered_set` behaviour is oracle-differential-tested in
`../tests/differential/` (navigation, 10 001-key, matchspec, ...). No `#[ignore]`; no self-comparison
tautologies.

`../interior_bench.rs::ordered_interior_microbenchmark` is the reproducible B+-tree-vs-sorted-`Vec`
comparison (it asserts every measurement, so a wrong-but-fast index fails the run):
```text
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets \
    --lib ordered_interior_microbenchmark -- --nocapture
```
Use `BEAM_ETS_BENCH_SIZES=1000,100000 BEAM_ETS_BENCH_REPEATS=5` for stable figures (each cell is the
per-metric **minimum** over the passes) and `BEAM_ETS_BENCH_INDEX=bplus|sorted` to run one interior.
Absolute ns are meaningless on a shared host — read the printed `sorted/tree` ratio column. On such a
run the B+-tree wins exactly where a `Vec`'s tail shift bites: at n = 1e5 the sorted `Vec` is
9.6–36.0× slower inserting and 10.3–29.8× slower deleting, while it stays ahead on the
comparison-light work — 0.27–0.33× the iteration time and 0.63–1.18× / 0.90–1.56× on hit/miss
lookup — and its index footprint is the smaller one (16.4–21.0 vs 28.7–29.6 bytes/entry, the price of
the internal nodes that buy the `O(log n)` mutation).

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
