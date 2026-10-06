# `beam_ets` — ETS Tables (Stage 3 data structures)

## Intent
Owns the **observable semantics of `ets:*`** — Erlang Term Storage — over **flat, table-owned term
storage** (the A.1 "copy boundary"). Implements Part I §A.1 (term/key/value semantics) and §A.8
(`ets:info/1` introspection) and the Stage-3 row of the plan (Part II §H): `set`, `ordered_set`,
`bag`, `duplicate_bag` over hash-indexed and B+-tree interiors.

The **interior data structures are NOT externally pinned** (crate-layer `../CONTEXT.md` §2): bucket
layout, hash-map thresholds, tree fan-out and load factors are implementation choices. Only the
**semantics** are contract — which object wins on a duplicate key, key equality/ordering, `size`, and
what `lookup`/`select`/`tab2list`/`first`/`next` return. Term **ordering** and **hashing** are owned
by `beam_term` (`compare::cmp`/`eq`, `hashing::term_hash`) and MUST NOT be re-implemented here.

Legacy ground truth (read-only): `../../erts/emulator/beam/erl_db.c` (API), `erl_db_hash.c`
(hash tables: `set`/`bag`), `erl_db_tree.c` (`ordered_set`), `erl_db_util.c` (key/term handling, the
match compiler), headers `erl_db.h`/`erl_db_hash.h`/`erl_db_tree.h`. Mirror the SEMANTICS, not the
layouts.

## Position & dependencies
Crate-layer **wave 1** (`../CONTEXT.md`). Depends only on `beam_common` (errors), `beam_term` (terms
+ ordering/hashing), `beam_alloc` (table-owned allocation) and `libc`. Consumed by `beam_bifs` (the
`ets:*` BIFs) and `beam_rt`. No new dependency edges may be added without reporting to `../`.

The **atom table is a per-call parameter.** Every op that must compare or hash a term takes
`atoms: &AtomTable`. There is one live, global atom table in the runtime (`beam_rt` owns it); a
`Table` does not own an `AtomTable` (indices must stay global) and stores no atom state.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_ets::prelude::*`.

### `kind.rs` — kinds, access, options
- `pub enum TableKind { Set, OrderedSet, Bag, DuplicateBag }` (`Default = Set`);
  `as_atom_name() -> &'static str`, `from_atom_name(name) -> Option<TableKind>`,
  `is_multi() -> bool` (bag/duplicate_bag), `is_ordered() -> bool` (ordered_set).
- `pub enum Access { Public, Protected, Private }` (`Default = Protected`); `as_atom_name()`,
  `from_atom_name(name) -> Option<Access>`.
- `pub enum Concurrency { None, Read, Write, ReadWrite }`; `as_atom_name()`,
  `from_atom_name(name) -> Option<Concurrency>` — **advisory** (parsed/reported, not enforced; see
  Known Issues).
- `pub struct TableOptions` (builder; defaults: `Set`, keypos 1, `Protected`, unnamed, uncompressed,
  no concurrency): `new()`, `set_kind`, `set_keypos(usize) -> Result<&mut Self>` (rejects 0),
  `set_access`, `set_named(bool)`, `set_name(Atom)`, `set_compressed(bool)`,
  `set_write_concurrency(bool)`, `set_read_concurrency(bool)`; getters `kind()/keypos()/access()/
  is_named()/name() -> Option<Atom>/is_compressed()/write_concurrency()/read_concurrency()/
  concurrency() -> Concurrency`; `impl Default`. `set_name` records the registered name **only** —
  it does not set the `named_table` flag (that is `set_named`), mirroring the oracle where
  `ets:new(myanon, [])` reports `name => myanon` but `named_table => false`.
- `pub fn can_read(access: Access, owner: Option<OwnerId>, requester: Option<OwnerId>) -> bool` and
  `pub fn can_write(...) -> bool` — the access matrix (`requester == owner` ⇒ everything).

### `key.rs` — object/key handling (the DB key rules)
- `pub fn key_of(object: Eterm, keypos: usize) -> Result<Eterm>` — an object **must be a tuple** with
  `arity >= keypos`; returns element `keypos` (1-based). Non-tuple / short arity / `keypos == 0` ⇒
  `Err(BeamError::BadArg)`.
- `pub fn is_valid_object(object: Eterm, keypos: usize) -> bool`.
- `pub unsafe fn object_elements<'a>(obj: Eterm) -> &'a [Eterm]` — the tuple payload slice (no header)
  handed to `select` predicates. `# Safety`: `obj` must be a live tuple.

### `store/mod.rs` — table-owned term storage (the copy boundary)
- `pub struct TermStore` — a **never-relocating bump arena** over a `beam_alloc::AllocRef`
  (`AllocType::Ets`) with **per-object reclamation**.
  `new(alloc: AllocRef) -> TermStore`, `with_default_allocator() -> TermStore`,
  `copy(&mut self, root: Eterm) -> Result<Eterm>` (deep-copies a term graph into table-owned memory;
  immediates copy by value; a rejected copy rolls back), `free(&mut self, root: Eterm) -> usize`
  (reclaims one copied root's exact-word-size runs to per-size free lists for reuse),
  `relinquish(&mut self, root: Eterm) -> usize` (the `ets:take/2` "check out": the object stays
  **valid** — its runs are never handed out again — but stops counting toward `words()`; unlike
  `free` it does **not** return the runs to the free lists), `reset()`
  (releases every block), `words() -> usize` (live words: handed out by `copy` and not yet `free`d or
  `relinquish`ed), `blocks() -> usize`,
  `memory() -> usize` (bytes held). `impl Drop` frees every block.
- Deep-copy **supported subset**: immediates, cons/list cells, tuples, **flat maps**, and the
  *pointer-free* boxed leaves (float, bignum, internal ref, on-heap bitstring).
  **Unsupported** (⇒ `Err(BeamError::BadArg)`): funs, hash-maps, records, external pid/port/ref,
  off-heap bin refs/sub-bitstrings. See Known Issues.

### `table/mod.rs` — the `Table` handle + ops
- `pub struct TableId(u64)`: `new(u64)`, `raw(self) -> u64`, `next() -> TableId` (process-global
  monotonic), `Display`. `pub struct OwnerId(u64)`: `new(u64)`, `raw(self) -> u64`.
- `pub struct Table`: metadata + table-owned `TermStore` + one interior.
```rust
impl Table {
    pub fn new(id: TableId, opts: TableOptions) -> Result<Table>;
    pub fn with_allocator(id: TableId, opts: TableOptions, alloc: AllocRef) -> Result<Table>;

    // metadata
    pub fn id(&self) -> TableId;
    pub fn kind(&self) -> TableKind;
    pub fn name(&self) -> Option<Atom>;      // Some iff a name was recorded (independent of named_table)
    pub fn keypos(&self) -> usize;           // 1-based
    pub fn access(&self) -> Access;
    pub fn owner(&self) -> Option<OwnerId>;
    pub fn set_owner(&mut self, owner: Option<OwnerId>);
    pub fn info(&self) -> TableInfo;

    // writes (object = the whole tuple; key derived from keypos)
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()>;
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool>; // false if key exists
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()>; // ATOMIC: validates the whole list first
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()>;          // ets:delete/2 (by key)
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()>;
    pub fn delete_all_objects(&mut self);

    // reads
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm>;              // 0/1 for set/ordered_set
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> bool;
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm>; // scalar; `unsupported` raise for bag/duplicate_bag
    pub fn lookup_element_or(&self, key: Eterm, pos: usize, default: Eterm, atoms: &AtomTable) -> Result<Eterm>; // scalar; absent key ⇒ default, `unsupported` raise for multi kinds
    pub fn lookup_element_all(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Vec<Eterm>>; // the Pos-th element of EVERY object under key (the multi kinds' list result)
    pub fn lookup_element_all_or(&self, key: Eterm, pos: usize, default: Eterm, atoms: &AtomTable) -> Result<Option<Vec<Eterm>>>; // Ok(None) ⇒ caller yields `default`
    pub fn memory(&self) -> usize;                                                  // store words + interior words
    pub fn size(&self) -> usize;                                                    // objects (not keys) for bag/dup_bag
    pub fn is_empty(&self) -> bool;
    pub fn tab2list(&self) -> Vec<Eterm>;                                           // ordered for ordered_set

    // element updates
    pub fn update_counter(&mut self, key: Eterm, update_op: Eterm, atoms: &AtomTable, heap: &mut beam_alloc::Heap) -> Result<Eterm>;
    pub fn update_counter_with(&mut self, key: Eterm, update_op: Eterm, default: Eterm, atoms: &AtomTable, heap: &mut beam_alloc::Heap) -> Result<Eterm>;
    pub fn update_element(&mut self, key: Eterm, updates: &[(usize, Eterm)], atoms: &AtomTable, heap: &mut beam_alloc::Heap) -> Result<bool>;

    // navigation (None == '$end_of_table')
    pub fn first(&self, atoms: &AtomTable) -> Option<Eterm>;
    pub fn last(&self, atoms: &AtomTable) -> Option<Eterm>;
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>>;
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>>;
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>>; // copies out + reclaims the stored originals

    // access control
    pub fn check_read(&self, requester: Option<OwnerId>) -> Result<()>;
    pub fn check_write(&self, requester: Option<OwnerId>) -> Result<()>;
    pub fn as_requester(&mut self, requester: Option<OwnerId>) -> TableAccess<'_>;

    // select family (see select/exec.rs; see also the predicate primitive)
    pub fn select_all(&self, pred: &mut SelectPredicate<'_>) -> Vec<Eterm>;
    pub fn select(&self, spec: &MatchSpec, atoms: &AtomTable, heap: &mut Heap) -> Result<Vec<Eterm>>;
    pub fn select_delete(&mut self, spec: &MatchSpec, atoms: &AtomTable, heap: &mut Heap) -> Result<usize>;
    pub fn select_count(&self, spec: &MatchSpec, atoms: &AtomTable, heap: &mut Heap) -> Result<usize>;
    pub fn match_object(&self, pattern_term: Eterm, atoms: &AtomTable, heap: &mut Heap) -> Result<Vec<Eterm>>;
    pub fn match_(&self, pattern_term: Eterm, atoms: &AtomTable, heap: &mut Heap) -> Result<Vec<Eterm>>;
    pub fn match_delete(&mut self, pattern_term: Eterm, atoms: &AtomTable, heap: &mut Heap) -> Result<usize>;
}
```
- `pub(crate) trait Interior` — the internal dispatch seam implemented by the two interiors
  (`insert`/`insert_new`/`delete`/`delete_object`/`lookup`/`member`/`lookup_element`/`lookup_elements`/`size`/
  `iter_objects`/`keys`/`take`/`first_key`/`last_key`/`next_key`/`prev_key`/`memory`/
  `update_counter`/`update_element`). `Table` stores `Box<dyn Interior>`. `insert` returns
  `Result<bool>` — **whether the object was retained** — so `Table::insert` reclaims the deep copy it
  made when the interior rejects it as a duplicate (`bag` is the only kind that can return `false`;
  `set`/`ordered_set`/`duplicate_bag` always retain). Generic defaults rebuild the
  object on the caller's heap (what the B+-tree needs); the hash interior overrides the element
  updates with **destructive in-place writes**. Private — **not** part of the cross-crate API.

### `table/access.rs` — the access-enforcing view
- `pub struct TableAccess<'a>` (from `Table::as_requester`) — rechecks `can_read`/`can_write` on
  every op and raises `BeamError::BadArg` on a violation, exactly like `ets`. It wraps the **whole**
  observable `ets` surface: `requester() -> Option<OwnerId>`; write-checked `insert`, `insert_list`,
  `insert_new`, `delete`, `delete_object`, `delete_all_objects`, `take -> Result<Vec<Eterm>>`,
  `update_counter`, `update_counter_with`, `update_element`, `select_delete`, `match_delete`; and
  read-checked `lookup -> Result<Vec<Eterm>>`, `member -> Result<bool>`, `lookup_element`,
  `lookup_element_or`, `lookup_element_all`, `lookup_element_all_or`, `select`, `select_count`, `match_object`, `match_`, `first`/`last`/`next`/`prev
  -> Result<Option<Eterm>>`, `tab2list -> Result<Vec<Eterm>>`, `size -> Result<usize>`,
  `is_empty -> Result<bool>`. `first`/`last`/`next`/`prev` are table-level, so the same read-checked
  wrappers cover every kind (`ordered_set`'s interior overrides the underlying `first_key`/`last_key`/
  `next_key`/`prev_key` with exact B+-tree searches, but the access layer sits above the `Table`).
  The underlying `Table` methods stay owner-level primitives (no access check). The matrix is
  verified against the system `erl` (OTP 29 / erts-17.1).

### `table/update.rs` — `ets:update_counter`/`update_element` operand machinery
- `pub(crate)` helpers shared with the hash interior: `counter_slot`, `add_counters`,
  `replace_elements` (re-exported from `table/mod.rs` so the `crate::table::<fn>` paths hold);
  internals `object_arity`, `read_counter`, `counter_operand`, `counter_operand_error`, `warp_fires`,
  `decode_counter_ops`, `decode_counter_tuple`, `build_list`, `CounterOp`.

### `hash_interior.rs` — hash-indexed interior (`set`/`bag`/`duplicate_bag`)
- `pub struct HashInterior`: buckets `Vec<Vec<Entry>>` (`Entry { key, objs: Vec<Eterm> }`), lookup by
  `term_hash(key) % buckets` then `eq`; doubles + rehashes past a load factor. `set` keeps ≤ 1 object
  per key; `bag` rejects a duplicate *object* within a key via `eq` (its `Interior::insert` returns
  `false`, so the caller reclaims that rejected copy); `duplicate_bag` keeps a multiset.
  Public API for testing/reuse: `new(kind: TableKind)`, `take(key, atoms) -> Vec<Eterm>`,
  `update_counter(key, pos, incr, atoms) -> Result<Eterm>` (set/ordered_set semantics; in-place),
  `update_element(key, updates, atoms) -> Result<bool>`, `memory(atoms) -> usize`.
  It implements `keys()` (bucket-scan order) but **not** `first_key`/`last_key`/`next_key`/`prev_key`,
  so the trait defaults supply them (see Design Decisions, hash-kind navigation order).

### `ordered_interior/` — the K-ary B+-tree interior (`ordered_set`)
- `mod.rs` — `pub struct OrderedSetInterior` (key-ordered B+-tree, see Design Decisions): `new()`,
  `first`/`last`, `first_key`/`last_key`, `next`/`prev`, `next_from(bound, inclusive)`/
  `prev_from(bound, inclusive)` (the partial-bound range searches) — all via `beam_term::compare::cmp`.
  Plus its `Interior` impl (which overrides `first_key`/`last_key`/`next_key`/`prev_key` and
  `memory`). Interior storage (the node arena) **is** counted by `memory()`.
- `node.rs` — the arena node layout + `ORDER`/`MIN_OCCUPANCY` constants and the `NodeStore`
  (`NodeId` slots + free list; nodes never relocate).
- `tree.rs` — insert/lookup/delete with split + borrow-or-merge rebalance and the separator invariant
  `keys[i] == max(child[i])`.
- `iter.rs` — the leaf-chain ordered iterator.

### `select/` — the match-spec compiler + executor
- `mod.rs` — `pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;`
  `pub struct MatchSpec { pub clauses: Vec<MatchClause> }`,
  `pub struct MatchClause { head, guard, body }`, `pub fn compile(spec: Eterm, atoms: &AtomTable) -> Result<MatchSpec>`;
  type aliases `MatchPattern`/`MatchElement` = `Pattern`, `MatchGuard`/`MatchBody`/`MatchBodyElement` = `Expr`.
- `pattern.rs` — `pub enum Pattern` (nested tuple/list/improper patterns, quoted literals, `'_'`,
  `'$N'` binds with repeat-occurrence equality, `'$_'`/`'$$'` which never match in a head).
- `expr.rs` — `pub enum Expr`, `pub enum CmpOp`, `pub enum TypeTest`: the guard/body expression
  language. Guards: `andalso`/`and`, `orelse`/`or`, `not`, `const`, bare truthiness; comparisons
  `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=` (structural `cmp` for exact, `cmp_arith` for `==`-family);
  type tests `is_atom/integer/float/number/tuple/list/binary/bitstring/map/pid/port/reference/function/boolean`;
  guard BIFs `element/2`, `abs/1`, `size/1`, `hd/1`, `tl/1`, `length/1`, `self/0`. Bodies: sequences
  (value = last expression), tuple/list construction, literals, `'$N'`, `'$_'`, `'$$'`. Guard truth
  is an internal `Expr::Bool` (independent of whether `true`/`false` are interned).
  Anything outside the recognised set ⇒ `compile`/`expr` `badarg` (Known Issues).
- `exec.rs` — the `impl Table` block for the select family + `pub enum BodyKind`
  (`WholeObject`/`AllBindings`/`LiteralTrue`) that reuses the compiler for `ets:match*`.

### `info.rs` — introspection (`ets:info`)
- `pub struct TableInfo { id, name: Option<Atom>, kind, keypos, access, size, memory, owner:
  Option<OwnerId>, named_table: bool, compressed: bool, write_concurrency: bool, read_concurrency: bool }`;
  `pub fn entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)>` — the `{Key, Value}` pairs
  (`id`/`name`/`type`/`keypos`/`protection`/`size`/`memory`/`owner`/`named_table`/…) the BIF layer
  turns into the `ets:info/1` list. Heap-free.

### `error.rs` — failure constructors
- `pub(crate) fn badarg() -> BeamError` (and siblings). No crate-local error enum.

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`. `badarg`-style failures ⇒ `BeamError::BadArg`;
  never `panic!`/`unwrap()` on runtime data; no crate-local error enum. Table ops never panic on
  malformed input — they return `Err`.
- **Never re-implement term order/hash.** Ordering ⇒ `beam_term::compare::cmp`/`eq`; hashing ⇒
  `beam_term::hashing::term_hash` (workspace Prevent-Duplication rule).
- **Table-owned storage only (A.1 copy boundary).** An inserted object is **deep-copied** into the
  table's `TermStore`; the table never retains a pointer into a caller's/process heap. Objects/keys
  returned from reads alias table storage and stay valid for the table's lifetime.
- **`unsafe` confinement.** `unsafe` is confined to `store/` (raw term reads/writes), the
  tuple-element accessor in `key.rs`, and the small term-reading blocks in `select/` and
  `table/mod.rs` (`select_all`). Every `unsafe fn` carries a `# Safety` section and every `unsafe`
  block a preceding `// SAFETY:` rationale (`beam_common::safety`); `unsafe_op_in_unsafe_fn` stays
  clean. `kind`/`info`/`error`/`hash_interior`/`ordered_interior` and most of `table/` are safe Rust.
- **Files ~1000 lines**, one responsibility per file.
- **Licensing** Apache-2.0; no new external deps (only `beam_common`/`beam_term`/`beam_alloc`/`libc`).

## Design Decisions
- **`ordered_set` is a K-ary B+-tree** (`ORDER = 32`, `MIN_OCCUPANCY = ORDER/2 = 16`), replacing the
  earlier sorted-`Vec` placeholder. Nodes live in an arena-backed `NodeStore` addressed by `NodeId`
  (with a free list), so **nodes never relocate** and no interior pointer is ever invalidated —
  matching `erl_db_tree.c`'s shape and giving O(log n) insert/lookup/delete and O(log n) ordered
  iteration via a leaf chain plus the separator invariant `keys[i] == max(child[i])`. Rebalancing is
  borrow-or-merge on underflow. All key comparisons go through `beam_term::compare::cmp` (so `1` and
  `1.0` are distinct keys). Recorded as a structural change for `../../CHANGE.md` (repo-root owned).
- **Interiors are a private `Interior` trait, not a pinned layout.** The workspace §2 rule says the
  interior is open; the trait lets the shape change without touching `Table` or any dependent. It
  carries **generic defaults** (rebuild-on-heap) with **hash overrides** (in-place destructive
  element writes) so the B+-tree needs no writable index slot.
- **One `TermStore` per table; copy-on-insert, per-object free.** Term graphs are deep-copied into a
  never-relocating bump arena over a `beam_alloc` `AllocRef` (`AllocType::Ets`); `free(root)` returns
  a removed root's exact-word-size runs to per-size free lists that `copy` reuses before bumping. It
  is wired into `insert` (replace), `delete`, `delete_object` and `delete_all_objects` (`reset`).
  This is the A.1 copy boundary realized honestly — **not** a `beam_message::copy_struct` reuse (that
  crate is wave 2; `beam_ets` is wave 1 and cannot depend upward).
- **`take` returns checked-out copies and reclaims the stored originals.** `Table::take` deep-copies
  each removed object out of the store (`TermStore::copy`) and `TermStore::free`s the table's original,
  then `relinquish`es each returned copy: the copy stays valid for the table's lifetime and its words
  are never handed out again, but it stops counting toward `Table::memory()` — matching real
  `ets:take/2`, whose objects leave the table. Relinquished words remain held by their block until
  `delete_all_objects`/drop (the arena never shrinks). Copy-before-free is mandatory: freeing first
  would let a later same-size `copy` reuse the words the caller now holds.
- **Hash-kind navigation order is unspecified (a deliberate freedom, not a gap).** `HashInterior`
  supplies the bucket-scan `keys()` and relies on the `Interior` trait defaults for
  `first_key`/`last_key`/`next_key`/`prev_key`; real ETS likewise does not specify hash traversal order
  (real `erl` returns `first` for `last` on a small set). Only the *order* is unspecified: every key a
  walk yields is a present key, `first`+`next` is a permutation of the stored keys, and `next`/`prev`
  on an **absent** anchor are `badarg`. `ordered_set` navigation is exact (ascending `beam_term` order;
  absent keys read as range bounds).
- **Access control mirrors `db_check_access`.** `can_read`/`can_write` treat `requester == owner` as
  full access and otherwise apply the `public`/`protected`/`private` matrix; `TableAccess` is the
  enforcing view while the raw `Table` ops stay owner-level primitives.
- **Guard truth is an internal boolean (`Expr::Bool`)**, not the interned atoms `true`/`false`, so a
  guard's truth value never depends on the `AtomTable`'s contents. (Boolean *bodies* still render as
  the atoms `true`/`false`, which must be interned in `atoms`.)
- **`match_delete` returns the deleted count, not the BIF's `true`.** The count is the only extra
  information the operation can report; the BIF layer renders the observable `true`. Differential
  renders the ETS-observable result.
- **`&AtomTable` is passed per call.** A `Table` cannot own atom state (indices are global and owned
  by the runtime); threading `&AtomTable` matches `beam_term`'s `cmp`/`eq`/`term_hash` signatures.
- **`size` counts objects, not keys.** For `bag`/`duplicate_bag` a key may hold many objects; `size`
  returns the object count (matching `ets:info(T, size)`), which differs from the entry count.

## Test Strategy
- Per-module `#[cfg(test)] mod tests` in each `src/` module (`store/tests.rs` holds `TermStore`'s,
  split out to keep `store/mod.rs` small): kind/options, key validation, store copy/free/reuse and
  `relinquish` (check-out), `Table`-level insert/take store-word accounting, hash-interior ops
  (incl. the `insert`-retained-bool), B+-tree invariants (incl. 10 000-key churn), select
  compile/execute, info. 84 unit tests.
- Crate-level integration over the **public API only**, split by concern: `tests/ets_basic.rs`
  (incl. `take` reclamation + churn), `ets_order.rs`, `ets_select.rs`, `ets_bag.rs` (incl. the
  duplicate-insert / `insert_new` copy-leak checks), `ets_info.rs`, `ets_nav.rs` (incl. the
  order-independent hash-navigation contract), `ets_access.rs`, `ets_update.rs`, sharing helpers from
  `tests/common/mod.rs`; plus `tests/select.rs` for the match-spec subset. 53 ETS tests + 18 select
  tests.
- **Differential evidence suite** (the compatibility gate): `tests/differential.rs` drives 61 cases
  (`tests/differential/battery/*.rs`) identically under the **system `erl`** (OTP 29 / erts-17.1, the
  embedded program in `tests/differential/erl_program.rs`) and under `beam_ets`, comparing canonical
  renderings line-by-line (atoms/small ints/tuples/proper lists; order-insensitive values sorted;
  table ids/`memory`/pids/floats/binaries never rendered). Coverage: all 4 kinds × 8 core ops,
  `{keypos,2}`, ordered navigation, `update_counter/3,4` + `update_element`, `take`/`lookup_element`,
  the select/match family with guards, malformed specs, and >10 000-key tables. `KNOWN_DIVERGENCES`
  is empty. Without `erl` the live-oracle test skips; `BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into
  a failure (use it in CI).
- **Gates (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets` and
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --workspace`.
  Forced-oracle: `BEAM_ETS_REQUIRE_ORACLE=1 erts_rust/scripts/cargo.sh test --manifest-path
  erts_rust/Cargo.toml -p beam_ets --test differential`.

## Known Issues (honest gaps — not full parity)
- **`Concurrency` is advisory.** `read_concurrency`/`write_concurrency` are parsed and reported but
  there is no locking or per-reader table; a `Table` is single-threaded.
- **Named-table lookup-by-name is the runtime's job, not this crate's.** `Table` exposes `name()`/
  `is_named()` only; the name→table registry is the sibling `beam_bifs::ets::TableRegistry` (this
  crate holds no global state, so it must not duplicate that registry).
- **Deep-copy is a subset.** Funs, hash-maps, records, external pid/port/ref and off-heap bin
  refs/sub-bitstrings cannot be copied into table storage (`insert` ⇒ `BadArg`).
- **The match-spec compiler covers the common guard set**, not the full Erlang guard language
  (`map_get`, `float`/`trunc`/`round`, `list_to_tuple`, comprehensions in bodies, …); anything
  outside the recognised set is `badarg`.
- **`ets:slot`-style iteration and `tab2file`/`file2tab` are not implemented** (out of scope for this
  pass); `tab2list` plus `first`/`next` cover enumeration.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, `Access`, `Concurrency`, `TableOptions`, `can_read`/`can_write`
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store/mod.rs` → `TermStore` (table-owned arena + copy/free/reuse) — the copy boundary
- `src/store/tests.rs` → the `TermStore` unit-test module
- `src/table/mod.rs` → `Table`/`TableId`/`OwnerId`, the `Interior` trait, op dispatch, navigation, `select_all`, metadata
- `src/table/access.rs` → `TableAccess` (access-enforcing view)
- `src/table/update.rs` → `update_counter`/`update_element` operand machinery (shared with the interiors)
- `src/hash_interior.rs` → `HashInterior` (bucket hash index) for `set`/`bag`/`duplicate_bag`
- `src/ordered_interior/mod.rs` → `OrderedSetInterior` (B+-tree) for `ordered_set` + its `Interior` impl
- `src/ordered_interior/node.rs` → node layout, `ORDER`/`MIN_OCCUPANCY`, `NodeStore` arena
- `src/ordered_interior/tree.rs` → B+-tree insert/lookup/delete + rebalance
- `src/ordered_interior/iter.rs` → leaf-chain ordered iterator
- `src/select/mod.rs` → `MatchSpec`/`MatchClause`/`SelectPredicate` + `compile`
- `src/select/pattern.rs` → head patterns (`Pattern`)
- `src/select/expr.rs` → guard/body expression language (`Expr`/`CmpOp`/`TypeTest`)
- `src/select/exec.rs` → `impl Table` select family (`select`/`select_delete`/`select_count`/`match*`) + `BodyKind`
- `src/info.rs` → `TableInfo` + `entries` (the `ets:info` pairs)
- `src/error.rs` → `badarg`/failure constructors (no new error enum)
- `tests/common/mod.rs` → shared integration-test helpers (one home, no duplication)
- `tests/ets_*.rs` → public-API integration tests by concern (basic/order/select/bag/info/nav/access/update)
- `tests/select.rs` → the match-spec compiler integration tests
- `tests/differential.rs` + `tests/differential/{battery/*,erl_program.rs}` → the differential evidence suite vs the system `erl`
- `../../erts/emulator/beam/erl_db.c` → the API + trap-loop semantics being mirrored (read-only)
- `../../erts/emulator/beam/erl_db_hash.c` / `erl_db_tree.c` / `erl_db_util.c` → the interiors + key/match handling being mirrored (read-only)
- `../../erts/emulator/beam/erl_db.h` / `erl_db_hash.h` / `erl_db_tree.h` → flags/fields (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the `Table` seam, dependency waves, milestone scope.
- `../../CONTEXT.md` → workspace contracts (§1 A.1/A.8, §2 frozen term layout, §3 error strategy, §7).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`hashing` utilities.
- `../beam_alloc/CONTEXT.md` → the `AllocRef`/`AllocType::Ets` allocation seam `TermStore` uses.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §B/§D → data-structure and ETS copy-trimming discussion.
