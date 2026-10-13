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
  `match_delete`/`select_replace`); tagged `take`; fixation; ownership (`heir -> Option<HeirView>`,
  `set_heir`/`set_owner`/`give_away`/`setopts`); and `as_requester` (the raw access-enforcing view).
- `iter.rs` — view-borrowed traversal returning `TableTerm<'a>` bound to the borrow
  (`iter_keys`/`iter_keys_rev`/`iter_objects`/`iter_objects_rev`), `foldl`/`foldr` (accumulator
  escapes, the object term does not), and the bulk copy-out `tab2list_into`; plus its unit tests.

## Constraints
- Term reads return `TableTerm<'g>`; the only ways to an owned `Eterm` are `copy_into` and the
  copy-out select forms (a deep copy into a caller-owned `TermStore`).
- Errors are `beam_common::Result`/`BeamError`; the scratch retry loop retries **only**
  `Alloc(Heap)` exhaustion and returns every other error untouched (no swallowing).

## Known Issues (a deliberate escape hatch)
- `TableWrite::as_requester` (write.rs) is the crate's **raw access-enforcement seam**, and it is
  deliberately **not** lifetime-tagged: it exists only so an access-checked operation is reachable
  from a shared-table view (the tagged `TableRead` is owner-level, with no `requester` concept), and
  its READ results are bare `Eterm`s aliasing table storage — a caller can return one out of a
  `with_write` closure (`T = Result<Vec<Eterm>>`, independent of the view lifetime), and a later
  same-word-size insert silently recycles its words. The airtight path for reads stays
  `TableRead`/`TableTerm` + `copy_into`, and only what is copied out under the guard may be kept.
  Pinned as the residual hazard by `as_requester_reads_are_a_documented_residual_hazard`.
- Views are `!Send` in fact — but because `std`'s `RwLockReadGuard`/`RwLockWriteGuard` are `!Send`
  (and `TableRead` also carries a `!Send` `beam_alloc::Heap`), **not** because "they carry a
  scratch Heap": `TableWrite` has no scratch heap. No test pins the views' `!Send`.

## Test Strategy
- Inline `#[cfg(test)]`: `tests.rs` (the hazard + fix, view reads/writes, exact shapes) and
  `iter.rs`'s own module (traversal orders, folds, `tab2list_into` churn survival). The
  compile-time contract is pinned by 4 `compile_fail` doctests in-node (term.rs ×1, iter.rs ×3)
  plus 1 on `SharedTable`. All are bare `compile_fail` (no `E0xxx` code) — they pass on *any*
  compile error; the imports resolve, so the only failure is the intended escape.
- Coverage gaps: none recorded — `TableRead::member` and `TableTerm::is_bitstring` are pinned by
  `member_reports_key_presence_across_kinds` / `bitstring_reads_distinguish_aligned_binaries_from_unaligned_bits`,
  and the `as_requester` residual hazard by `as_requester_reads_are_a_documented_residual_hazard`.

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
