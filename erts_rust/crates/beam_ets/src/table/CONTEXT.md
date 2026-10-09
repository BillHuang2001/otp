# `beam_ets::table` — the `Table` handle layer

## Intent
The `Table` handle: metadata (id/kind/keypos/access/owner/heir), the table-owned `TermStore`
(the A.1 copy boundary) and one interior behind the private `Interior` trait. Provides the
owner-level table primitives (`insert`/`delete`/`lookup`/navigation/element-updates/ownership),
the access-enforcing `TableAccess` view, the thread-safe `SharedTable` handle over
`Arc<RwLock<Table>>`, and the guard-scoped `TableRead`/`TableWrite`/`TableTerm` views. The
crate-level `CONTEXT.md` owns the deep A.1 copy-boundary narrative; this node owns the concrete
API and its honest limits.

## API Surface (one file = one concern)
- `mod.rs` — `Table` (constructors incl. explicit-interior variants, write ops, `give_away`/`setopts`,
  `info`, `select_all`, `check_read`/`check_write`), `TableId`, `OwnerId`; `NEXT_TABLE_ID`.
- `read.rs` — `Table` reads/navigation: `lookup`/`member`/`lookup_element(+_or/_all/_all_or)`,
  `first`/`last`/`next`/`prev`, `take`, `size`/`is_empty`/`tab2list`/`memory`.
- `rename.rs` — `Table::rename` + `RenameOutcome` (checked name write; registry rekey is the host's).
- `shared.rs` — `SharedTable` (`Arc<RwLock<Table>>`): `read`/`write`/`with_read`/`with_write`/
  `try_into_inner`; a poisoned lock is the typed `unsupported_ets: table lock poisoned`.
- `access.rs` — `TableAccess` (rechecks `can_read`/`can_write` per op; `badarg` on violation).
  `size`/`is_empty` are the **ungated** `ets:info(T, size)` projection (the oracle admits a
  non-owner of a `private` table); `give_away` has no protection check and `setopts`/`set_access`
  are owner-gated, exactly as in `ets:*`.
- `ownership.rs` — `Heir`/`SetOpt`, `install_heir`/`heir_data`/`copy_heir`/`replace_heir`.
- `lifecycle.rs` — `OwnerDeath` + `Table::owner_died` (the owner-death transition).
- `element.rs` — `Table::update_counter(+_with)`/`update_element` and `relocate_rebuilt_object`.
- `update.rs` — counter/element operand machinery (incl. an exact small/bignum add).
- `interior.rs` — the private `pub(crate) trait Interior` + `build_interior`.
- `view/{mod,read,write,term,iter}.rs` — the guard-scoped views (see `view/CONTEXT.md`).

## Concurrency (verified)
`SharedTable`'s lock is a **real** mutual-exclusion lock: a `write` guard spans a whole
check-then-act sequence (`insert_new`/`update_counter`/`select_replace`), so no update is lost.
`tests/ets_shared.rs`+`tests/ets_concurrency.rs` prove it with exact values (8 writers × 500 →
4000 objects; held-guard-blocks-writer non-vacuity check). Fine-grained `{write_concurrency}` is
advisory only (throughput, never results) — see the crate `CONTEXT.md`.

## Constraints
- Errors are `beam_common::{Result,BeamError}`; `badarg` for oracle failures, the typed
  `unsupported_ets: …` for genuine capability gaps. No crate-local error enum, no `unwrap` on
  runtime data.
- Term storage is table-owned only (A.1 copy boundary): inserts deep-copy in, every read aliases
  table storage (guard-scoped by the views).
- Term order/hash/size delegate to `beam_term` (`compare::{cmp,cmp_arith,eq}`, `size_object`) —
  never re-implemented here.
- `unsafe` only in `interior.rs`-adjacent term reads, `element.rs`/`update.rs` slot access and
  `select_all`; each has a `// SAFETY:` rationale.

## Known Issues (honest limits found by the Layer-1 honesty audit)
- **`TableWrite::as_requester` is an escape hatch** (`view/write.rs:147`): it returns
  `TableAccess<'_>`, whose reads (`access.rs:206,310,334,…`) return bare `Eterm`s aliasing table
  storage that a caller can return out of a `with_write` closure — the exact wrong-result hazard
  the views exist to prevent. Documented there as a "residual hazard"; it makes the "view is
  airtight" framing false (the accurate claim is "only `SharedTable`'s tagged reads are airtight").
- **`TableWrite::heir` leaks a bare aliasing term** (`view/write.rs:98`): it returns `Option<Heir>`
  whose public `data: Option<Eterm>` (`ownership.rs:135`) may be a table-owned copy (the
  `install_heir`/creation-time path), so a bare `Eterm` escapes the guard; `replace_heir`
  (`ownership.rs:218`) later frees those words. Not flagged in the `heir()` doc.
- **`do_update_counter` can early-return mid-loop** (`element.rs:230-249`): `interior.update_counter(..)?`
  inside the op loop propagates a failure (reachable only on scratch-heap exhaustion, since every
  op is pre-validated) **before** `relocate_rebuilt_object` runs, leaving the interior holding a
  caller-heap-built object — a dangling hazard the A.1 boundary forbids once that heap dies.
- **Stale doc comment**: `OwnerId` (`mod.rs:109`) still says "M1 skeleton: access is not yet
  enforced by owner", but `can_read`/`can_write` + `TableAccess` do enforce access by owner.
- **`compile_fail` doctests are bare** (no `E0xxx` codes): `view/term.rs:47`, `view/iter.rs:255,283,372`,
  `shared.rs:59`. Verified non-vacuous today (their imports resolve), but a future API rename could
  silently make them vacuous.
- `read.rs`'s `expect("scratch ensured above")` (`view/read.rs:342,364`) is a panic-on-invariant on
  a runtime path; `is_heap_exhaustion` (`view/mod.rs:97`) retries any `Alloc(Heap)` error, not
  provably only scratch exhaustion. Both bounded and non-swallowing.
- Untested public API: `TableRead::member` (`view/read.rs:163`), `TableTerm::is_bitstring`
  (`view/term.rs:149`).

## Notes for Agents
- The owner-death transition, heir semantics and `give_away`/`setopts` are **real** oracle-matched
  semantics, not placeholders; only the *delivery* of `{'ETS-TRANSFER',…}` / the table drop is the
  runtime's job (this crate has no pid/liveness concept — `OwnerId` is an opaque `u64`).
- `update_counter`/`update_element` arithmetic is **exact** (small↔bignum promotion, no truncation);
  it is the one corner of the crate where bignum math exists (the match-spec `ArithHook` default is
  small-int only). No `beam_term` LCA exposes big add/sub/compare, so it stays crate-local.
- No `#[ignore]`, no `todo!`/`unimplemented!`, no `#[allow(dead_code)]`, no `.ok()`/
  `unwrap_or_default` in this node; the single `Err(_)` (`shared.rs:215`) is the documented
  `Arc::try_unwrap`-non-unique ⇒ `Ok(None)` opt-out.

## Routing Table
- `mod.rs` → `Table` core + write ops + access checks + `select_all`; `TableId`/`OwnerId`
- `read.rs` → `Table` reads, navigation, `take`
- `rename.rs` → `Table::rename` (`ets:rename/2`)
- `shared.rs` → `SharedTable` (the lock handle)
- `access.rs` → `TableAccess` (access-enforcing view)
- `ownership.rs` → `Heir`/`SetOpt`/`install_heir`/`replace_heir`
- `lifecycle.rs` → `OwnerDeath`/`owner_died`
- `element.rs` → `update_counter`/`update_element`
- `update.rs` → counter/element operand machinery
- `interior.rs` → the private `Interior` trait + `build_interior`
- `view/` → the guard-scoped views (see `view/CONTEXT.md`)

## See Also
- `view/CONTEXT.md` → the lifetime-tagged views and their escape hatches.
- `../CONTEXT.md` → `beam_ets` intent, the A.1 copy boundary, frozen term/compare/hashing rules.
- `../ETS_SURFACE.md` → the `ets:*` coverage census and the host-side gaps.
