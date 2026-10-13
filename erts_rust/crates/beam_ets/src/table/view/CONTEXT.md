# Guard-Scoped Views (`beam_ets/src/table/view/`)

## Intent
The *safe* consumption path for table-owned terms. `TableRead`/`TableWrite` own the `RwLock`
guard over a `Table` and are the only producers of `TableTerm`, a term view whose lifetime is
tagged to the **borrow of the view** (not to the guard's own `RwLock*Guard<'g, _>` lifetime). A
term that outlives its view/closure is a compile error; owning a term needs the explicit
deep-copy `TableTerm::copy_into` (or the copy-out select family). This closes the A.1-read
aliasing hazard the crate `CONTEXT.md` records: a bare read aliases table storage whose words a
later same-size insert silently reuses (a wrong, well-formed result).

## API Surface
- `mod.rs` — module docs (the hazard + the fix), `SCRATCH_MIN_WORDS`/`SCRATCH_MAX_WORDS`, and
  `is_heap_exhaustion` (retry predicate: `BeamError::Alloc(kind == AllocKind::Heap)` only).
- `term.rs` — `TableTerm<'g>`: type tests (`is_*`), value accessors (`as_small_int`/`as_atom`/
  `as_float`/`tuple_arity`/`tuple_element`/`tuple_elements`/`list_to_terms`/`size`), comparisons
  (`eq_term`/`cmp_term`, which **delegate** to `beam_term::compare::{eq,cmp}`), and the copy-out
  `copy_into` (delegates to `TermStore::copy`). Constructor `new` is `pub(super)`; the raw
  `term: Eterm` field is private; no method returns a bare aliasing `Eterm`. It is `Clone + Copy`
  (one word + a ZST marker) so a tagged term can be handed back by value — both copies stay bounded
  by `'g`, so this does not widen the lifetime discipline.
- `term.rs` — `HeirView<'g>`: the installed heir read under a live write guard — owned
  `owner() -> OwnerId` plus `data() -> Option<TableTerm<'g>>`, the gift term tagged to the view's
  borrow so the table-owned copy cannot escape. `new(owner, Option<Eterm>)` is `pub(super)`; the
  only producer is `TableWrite::heir`.
- `read.rs` — `TableRead<'g>`: owned metadata; tagged term reads (`lookup`/`member`/
  `lookup_element`/`lookup_element_or`/`lookup_element_all`/`lookup_element_all_or`/`first`/
  `last`/`next`/`prev`/`slot`/`tab2list`); the **copy-out** select family (`select`/`select_count`/
  `match_object`/`match_`) over a lazily-allocated scratch `Heap`; `table()` is `pub(in view)`.
- `write.rs` — `TableWrite<'g>`: metadata; mutations (`insert`/`insert_new`/`insert_list`/
  `delete`/`delete_object`/`delete_all_objects`/`update_counter*`/`update_element`/`select_delete`/
  `match_delete`/`select_replace`); tagged `take`; fixation; ownership (`heir`/`set_heir`/`set_owner`/
  `give_away`/`setopts`); and `as_requester` (the access-enforcing view).
- `iter.rs` — view-borrowed traversal returning `TableTerm<'a>` bound to the borrow
  (`iter_keys`/`iter_keys_rev`/`iter_objects`/`iter_objects_rev`), `foldl`/`foldr` (accumulator
  escapes, the object term does not), and the bulk copy-out `tab2list_into`; plus its unit tests.

## Constraints
- Term reads return `TableTerm<'g>`; the only ways to an owned `Eterm` are `copy_into` and the
  copy-out select forms (a deep copy into a caller-owned `TermStore`).
- Errors are `beam_common::Result`/`BeamError`; the scratch retry loop retries **only**
  `Alloc(Heap)` exhaustion and returns every other error untouched (no swallowing).

## Known Issues (escape hatches — the layer is NOT fully airtight)
- `TableWrite::as_requester` (write.rs) hands out a `TableAccess<'_>` whose methods (`lookup`,
  `tab2list`, `first`, …) return bare `Eterm`s aliasing table storage; those can be returned out
  of a `with_write` closure (`T = Result<Vec<Eterm>>`, independent of the view lifetime). It is
  documented as a "Residual hazard", but it bypasses the lifetime discipline and can reproduce
  the very wrong-result hazard the views exist to prevent.
- `TableWrite::heir` returns `Option<Heir>` whose `data: Option<Eterm>` (`table/ownership.rs`)
  may be a **table-owned copy** (the `install_heir`/creation-time path), so a bare aliasing
  `Eterm` escapes the guard; replacing the heir later reclaims those words.
- Views are `!Send` in fact — but because `std`'s `RwLockReadGuard`/`RwLockWriteGuard` are `!Send`
  (and `TableRead` also carries a `!Send` `beam_alloc::Heap`), **not** because "they carry a
  scratch Heap": `TableWrite` has no scratch heap. No test pins the views' `!Send`.
- `read.rs` uses `expect("scratch ensured above")` (in `run_with_scratch`/`copy_out`) — a
  panic-on-invariant on a runtime path.

## Test Strategy
- Inline `#[cfg(test)]`: `tests.rs` (the hazard + fix, view reads/writes, exact shapes) and
  `iter.rs`'s own module (traversal orders, folds, `tab2list_into` churn survival). The
  compile-time contract is pinned by 4 `compile_fail` doctests in-node (term.rs ×1, iter.rs ×3)
  plus 1 on `SharedTable`. All are bare `compile_fail` (no `E0xxx` code) — they pass on *any*
  compile error; the imports resolve, so the only failure is the intended escape.
- Coverage gaps: `TableRead::member` and `TableTerm::is_bitstring` have no test.

## Routing Table
- `src/table/view/mod.rs` → module docs, scratch-heap sizing, the retry predicate
- `src/table/view/term.rs` → `TableTerm<'g>` (the lifetime-tagged term view)
- `src/table/view/read.rs` → `TableRead<'g>` (tagged reads + copy-out select)
- `src/table/view/write.rs` → `TableWrite<'g>` (mutations, `take`, ownership, `as_requester`)
- `src/table/view/iter.rs` → the view-borrowed traversal API + its unit tests
- `src/table/view/tests.rs` → the view-layer unit tests (hazard/fix/faithfulness)
- `../shared.rs` → `SharedTable` (the lock handle that produces the views)
- `../access.rs` → `TableAccess` (the raw, access-checked view — the bare-`Eterm` escape hatch)

## See Also
- `../CONTEXT.md` → the `Table` seam and the crate-level account of the guard-scoped views.
- `../../CONTEXT.md` → `beam_ets` intent, A.1 copy boundary, the frozen term/compare/hashing rules.
- `../../../beam_term/CONTEXT.md` → `compare`/`hashing`/`size` that `term.rs` delegates to.
