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
3. Every node below the root holds within `[MIN_OCCUPANCY=16, ORDER=32]` entries/children; the root
   leaf may be empty, a root branch keeps `>= 2` children.
4. All leaves are at the same depth. 5. The leaf chain visits every leaf exactly once, left to right.

## Design Decisions
- **Separators are always re-materialised from the rightmost live leaf** via
  `NodeStore::subtree_max_key`, never read back from `branch.keys` to construct one: an `ets:insert`
  upsert replaces the object a stored separator aliased and the caller reclaims that object's storage
  right after. Every mutation re-derives the separator of the child it descended into at every level
  (`tree.rs` `insert_rec`/`delete_rec`), plus after a split (`split_overfull`/`split_root`/
  `adopt_split`) and on a left borrow. `child_index` is the only production reader of `branch.keys`.
- `child_index` **binary-searches** the strictly-ascending separators (a hand-rolled lower bound: the
  first separator `>= key`, clamped to the last child) — the tree's hottest helper.
- Split at `ORDER+1`, cut at `ORDER/2` (16/17); rebalance borrows from a sibling with `> MIN`
  occupancy or merges (siblings then sum to `<= ORDER`), so occupancy stays bounded and the height
  changes only at the root, which collapses when it drops to a single child.
- A same-`cmp_arith`-key insert replaces **both** the object and the stored key term (the oracle's
  key-follows-object rule; probed against OTP 29).

## Constraints
- Every key comparison goes through `beam_term::compare::cmp_arith`; no `unsafe` in this directory.
- `expect`/`unreachable!` sit only on internal invariants (a `NodeId`/index the tree itself derived)
  and are unreachable from the public API; panics are never control flow for runtime data.
- The B+-tree assumes `cmp_arith` is a strict total order (its only caveat is the documented
  `beam_term` best-effort `f64` comparison for out-of-range bignum<->float keys).
- Files target ~1000 lines; the big test module lives in `mod.rs`.

## Test Strategy
`check_invariants` walks the whole tree against all five invariants (incl.
`separator[i] == max(child[i])`) after every step of scattered 200-key and 10 000-key
insert/delete/re-split churn, and drain-to-empty; an oracle `Vec` cross-checks `size` and the
ascending key run. `mod.rs` tests cover the empty case, replace/`insert_new`/`delete_object`,
mixed key classes, present/absent/boundary navigation, the replace-key-term rule, and memory
monotonicity; both interiors are driven through the `Interior` seam by shared helpers.
`../interior_parity.rs` proves the B+-tree and the sorted `Vec` produce identical observable
traces. End-to-end `ordered_set` behaviour is oracle-differential-tested in `../tests/differential/`
(navigation, 10 001-key, matchspec, ...). No `#[ignore]`; no self-comparison tautologies.

## Known Issues / Notes
- **`insert`/`delete` are `O(log^2 n)`, not `O(log n)`** as the module docs claim: the separator
  re-derivation descends to a subtree's rightmost leaf (`subtree_max_key`) at every level of both
  `insert_rec` and `delete_rec`. `find`/`next`/`prev`/`first`/`last` are genuinely `O(log n)`.
  Correctness-driven, not a bug — the `O(log n)` wording is the overstatement.
- `next_from`/`prev_from` with `inclusive = true` has no production consumer yet: only `Tree::next`/
  `prev` call them (`inclusive = false`). It is the range API a later `ets:select`/`ets:next`-style
  scan wires up (documented in `mod.rs`).
- The `borrowing_from_the_left_sibling_keeps_the_separator_exact` test comment (`mod.rs:851-853`)
  says the merged first leaf holds "30 entries"; the arithmetic gives 31 (15 + 16), and the merge
  happens inside the delete of 15, not a subsequent one. The scenario itself is correct; cosmetic.

## Routing Table
- `mod.rs` → `OrderedSetInterior` (B+-tree facade + `Interior` impl) and its test module
- `tree.rs` → `Tree`: insert/split, delete/borrow-or-merge, navigation, `check_invariants`
- `node.rs` → `Node`/`LeafNode`/`BranchNode`/`Entry`, the `NodeStore` arena, `search_entries`, `child_index`, `ORDER`/`MIN_OCCUPANCY`
- `iter.rs` → `LeafIter` (the leaf-chain ordered iterator)
- `sorted.rs` → `SortedOrderedInterior` (the sorted-`Vec` alternative) + its tests

## See Also
- `../../CONTEXT.md` (`beam_ets`) → the `Interior` seam, the crate API surface and the `ordered_set` interior entry.
- `../table/interior.rs` → the `Interior` trait both interiors implement.
- `../../../../erts/emulator/beam/erl_db_tree.c` → the legacy CATREE `ordered_set` being mirrored (read-only).
