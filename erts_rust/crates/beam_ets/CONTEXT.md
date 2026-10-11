# `beam_ets` — ETS Tables (Stage 3 data structures)

## Intent
Owns the **observable semantics of `ets:*`** — Erlang Term Storage — over **flat, table-owned term
storage** (the A.1 "copy boundary"): Part I §A.1 (term/key/value semantics) and §A.8 (`ets:info/1`
introspection), plus the Stage-3 row of the plan (Part II §H): `set` over a **CHAMP** index,
`bag`/`duplicate_bag` over a **chained-bucket** hash index, `ordered_set` over a **K-ary B+-tree**.

The **interior data structures are NOT externally pinned** (crate-layer `../CONTEXT.md` §2): bucket
layout, hash-map thresholds, tree fan-out and load factors are implementation choices. Only the
**semantics** are contract — which object wins on a duplicate key, key equality/ordering, `size`, and
what `lookup`/`select`/`tab2list`/`first`/`next` return. Term **ordering** and **hashing** are owned by
`beam_term` (`compare::cmp`/`cmp_arith`/`eq`, `hashing::term_hash`) and MUST NOT be re-implemented here.

Legacy ground truth (read-only): `../../erts/emulator/beam/erl_db.c` (API), `erl_db_hash.c`
(`set`/`bag`), `erl_db_tree.c` (`ordered_set`), `erl_db_util.c` (keys/match compiler), headers
`erl_db.h`/`erl_db_hash.h`/`erl_db_tree.h`. Mirror the SEMANTICS, not the layouts.
`ETS_SURFACE.md` is the `ets:*` coverage census (every operation → this crate / `beam_bifs` / missing,
plus the runtime-level gaps and the exact API each needs).

## Position & dependencies
Crate-layer **wave 1** (`../CONTEXT.md`). Depends only on `beam_common` (errors), `beam_term` (terms +
ordering/hashing), `beam_alloc` (table-owned allocation) and `libc`. Consumed by `beam_bifs` (the
`ets:*` BIFs) and `beam_rt`. No new dependency edges without reporting to `../`.
The **atom table is a per-call parameter**: every op that compares or hashes a term takes
`atoms: &AtomTable` (one live global table, owned by `beam_rt`; a `Table` stores no atom state).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_ets::prelude::*`.

### `kind.rs` — kinds, access, options
- `pub enum TableKind { Set, OrderedSet, Bag, DuplicateBag }` (`Default = Set`); `as_atom_name()`,
  `from_atom_name(name)`, `is_multi()` (bag/duplicate_bag), `is_ordered()`.
- `pub enum Access { Public, Protected, Private }` (`Default = Protected`); `as_atom_name()`,
  `from_atom_name(name)`.
- `pub enum Concurrency { None, Read, Write, ReadWrite }`; `as_atom_name()`, `from_atom_name(name)` —
  **advisory** (parsed/reported, not enforced).
- `pub enum WriteConcurrency { #[default] Off, On, Auto, FixedLocks(u32) }` — the `{write_concurrency,_}`
  mode (the plain bool is its `Off`/`On` projection): `is_enabled()`, `fixed_locks() -> Option<u32>`,
  `as_atom_name() -> Option<&'static str>` (`"false"`/`"true"`/`"auto"`; `None` for `FixedLocks`),
  `from_atom_name(name) -> Option<WriteConcurrency>`. `TableOptions` stores the mode; `FixedLocks(n)` keeps
  the **requested** count (the power-of-two / `>= 64` normalization is the parser's).
- `pub enum SetInterior { Chained, #[default] Champ }` and
  `pub enum OrderedInterior { #[default] BPlusTree, Sorted }` — the crate-internal interior selectors
  (`as_str()`); `Table::with_set_interior`/`with_ordered_interior` install them, `set_interior()`/
  `ordered_interior()` report them. Not `TableOptions` fields; no BIF surface.
- `pub struct TableOptions` (builder; defaults: `Set`, keypos 1, `Protected`, unnamed, uncompressed, no
  concurrency, no heir, no explicit `decentralized_counters`): `new()`, `set_kind`, `set_keypos(usize)
  -> Result<&mut Self>` (rejects 0 — the only fallible builder), `set_access`, `set_named(bool)`,
  `set_name(Atom)`, `set_compressed(bool)`, `set_write_concurrency(bool)`, `set_read_concurrency(bool)`,
  `set_write_concurrency_mode(WriteConcurrency)`, `set_heir(Option<Heir>)`,
  `set_decentralized_counters(Option<bool>)`; getters `kind()/keypos()/
  access()/is_named()/name() -> Option<Atom>/is_compressed()/write_concurrency()/read_concurrency()/
  write_concurrency_mode() -> WriteConcurrency/heir() -> Option<Heir>/decentralized_counters_option() ->
  Option<bool>/decentralized_counters() -> bool/concurrency() -> Concurrency`; `impl Default`.
  `write_concurrency()` is the effective `Off`/`On` projection of the mode (a `private` table forces
  `Off`); `decentralized_counters()` is `true` for `Auto`/`FixedLocks(_)` when the effective flag is on.
  Unit tests live in `src/kind/tests.rs`.
  `set_name` records the name only — it does not set `named_table` (the oracle: `ets:new(myanon, [])`
  reports `name => myanon` but `named_table => false`).
- `pub fn can_read(access, owner, requester) -> bool` / `can_write(…)` — the access matrix
  (`requester == owner` ⇒ everything). Verified against the system `erl`.

### `key.rs` — object/key handling (the DB key rules)
- `key_of(object, keypos) -> Result<Eterm>` — an object **must be a tuple** with `arity >= keypos`;
  non-tuple / short arity / `keypos == 0` ⇒ `Err(BeamError::BadArg)`.
- `is_valid_object(object, keypos) -> bool`.
- `pub unsafe fn object_elements<'a>(obj) -> &'a [Eterm]` — the tuple payload slice for `select`
  predicates. `# Safety`: `obj` must be a live tuple.

### `store/mod.rs` — table-owned term storage (the copy boundary)
- `pub struct TermStore` — a **never-relocating bump arena** over a `beam_alloc::AllocRef`
  (`AllocType::Ets`) with **per-object reclamation**: `new(alloc)`, `with_default_allocator()`,
  `copy(root) -> Result<Eterm>` (deep copy into table memory; a rejected copy rolls back),
  `free(root) -> usize` (reclaims one copied root's exact-word-size runs to per-size free lists for
  reuse; **a no-op returning 0 for a root this store never tracked**), `relinquish(root) -> usize`
  (the `ets:take/2` check-out: the object stays valid, its runs are never handed out again, but it
  stops counting toward `words()`), `reset()`, `words()`, `blocks()`, `memory()`. `impl Drop` frees
  every block.
- Deep-copy **covers every kind `ets:insert/2` accepts**: immediates, cons/list cells, tuples, flat
  **and hash** maps, funs (all three kinds), pointer-free boxed leaves (float, bignum, internal ref,
  on-heap bitstring), external pid/port/ref, and sub-bitstrings / off-heap refc binaries
  (re-materialised as self-contained on-heap bitstrings). The residual `BadArg`s (a bare `BinRef`
  root, a native record, the reserved boxed subtags, a bare header word, a malformed `ErlSubBits`)
  are never a user-visible inserted term — see Known Issues.

### `table/mod.rs` — the `Table` handle + ops
- `pub struct TableId(u64)`: `new(u64)`, `raw()`, `next()` (process-global monotonic), `Display`.
  `pub struct OwnerId(u64)`: `new(u64)`, `raw()`.
- `pub struct Table`: metadata + a table-owned `TermStore` + one interior behind the private
  `Interior` trait. Constructors `new(id, opts)`, `with_allocator(id, opts, alloc)`,
  `with_set_interior(id, opts, SetInterior)`, `with_ordered_interior(id, opts, OrderedInterior)`.
  Full signatures live in `src/table/{mod,read,access,element,ownership,lifecycle,rename}.rs`.
- **Shape conventions.** Every fallible op returns `beam_common::Result<_>`; `first`/`last`/`next`/
  `prev` return `Option` (`None` = `'$end_of_table'`). Unless noted an op takes `atoms: &AtomTable`;
  the select/match family and the element updates also take `heap: &mut Heap` (scratch space).
- **Metadata:** `id`, `kind`, `name() -> Option<Atom>` (Some iff a name was recorded), `keypos()`
  (1-based), `access`, `is_named_table() -> bool`, `owner()/set_owner()`, `heir() -> Option<Heir>` /
  `set_heir()` / `heir_data()`, `give_away(caller, new_owner)`, `setopts(caller, SetOpt)`,
  `set_access(&mut self, access: Access)` (post-creation protection change), `owner_died(dead, is_alive)
  -> OwnerDeath`, `info() -> TableInfo`, `rename(name, is_taken) -> Result<RenameOutcome>`.
- **Writes** (object = the whole tuple; key from `keypos`): `insert` (upsert), `insert_new -> bool`,
  `insert_list(&[object])` (**ATOMIC** — validates and deep-copies the whole list first), `delete(key)`,
  `delete_object(object)`, `delete_all_objects()`, plus the `pub(crate)` `replace_key(key, &[object])` —
  the storage-faithful key rebuild behind a `duplicate_bag` `select_replace/2` (validates and deep-copies
  the whole new list **before** removing the key's old objects, so a rebuild reclaims the objects it
  replaces instead of checking them out).
- **Reads:** `lookup(key) -> Vec<Eterm>` (0/1 for set/ordered_set), `member`, `lookup_element(key, pos)`
  / `lookup_element_or(key, pos, default)` (scalar; `unsupported` raise for the multi kinds),
  `lookup_element_all(key, pos) -> Vec<Eterm>` / `lookup_element_all_or(..) -> Option<Vec<Eterm>>` (the
  multi kinds' list result), `memory()`, `size()` (objects, not keys, for bag/dup_bag), `is_empty()`,
  `tab2list()` (ordered for ordered_set).
- **Element updates:** `update_counter(key, op)`, `update_counter_with(key, op, default)`,
  `update_element(key, &[(pos, term)]) -> bool`.
- **Navigation:** `first()`, `last()`, `next(key)`, `prev(key)`, `take(key) -> Vec<Eterm>` (copies out +
  reclaims the stored originals), plus the `pub(crate)` `tab2list_rev()` — the reverse-navigation walk
  (the `ordered_set` reverse of `tab2list()`; the hash kinds' `tab2list()` itself). For the **hash kinds**
  (`set`/`bag`/`duplicate_bag`) `last() == first()` and `prev(k) == next(k)` (`erl_db_hash.c:853-856`),
  with an unknown anchor `badarg` (`check_navigable`); only `ordered_set` has the real arithmetic order.
- **Access control:** `check_read(requester)` / `check_write(requester)` / `as_requester(requester) ->
  TableAccess<'_>`.
- **Select family** (`select/exec.rs`; also the `select_all(pred)` predicate primitive): `select(spec)`,
  `select_reverse(spec)`, `select_delete(spec) -> usize`, `select_count(spec) -> usize`, `match_object(pattern)`, `match_(pattern)`,
  `match_delete(pattern) -> usize`, `select_replace(spec) -> usize`. `select_reverse` (`exec.rs`) drives
  the interior's own reverse navigation (`Table::tab2list_rev`): for an `ordered_set` it is the exact
  reverse of `Table::tab2list()`, while for the **hash kinds** it returns the **same** list in the
  **same** order as `select` (their `last`/`prev` **are** `first`/`next`, `erl_db_hash.c:2310-2313`).
- **Limit + continuation forms** (`continuation.rs`; `Option::None` ⇒ `'$end_of_table'`): each
  `-> (Vec<Eterm>, Option<MatchContinuation>)`: `select_limit`/`select_continue`/`select_reverse_limit`/
  `select_reverse_continue`/`match_limit`/`match_continue`/`match_object_limit`/`match_object_continue`.
  `MatchContinuation` carries a private `Cursor` (`Begin`/`Anchor(key)`/`Position(ordinal)`) plus a
  private direction, so a `/1` resume keeps both: an `ordered_set` resumes from the last-yielded key as
  an **exclusive** `next`/`prev` range bound (`Anchor`), a hash kind from an ordinal (`Position`).
- `Table::set_fixation(bool)` / `is_fixed()` / `deferred_objects()` — `ets:safe_fixtable/2`.
- `pub fn match_spec_run(list, spec, atoms, heap) -> Result<Vec<Eterm>>` (`run.rs`) — the
  table-independent `ets:match_spec_run/2` runner.
- `pub(crate) trait Interior` — the internal dispatch seam (`insert`/`insert_new`/`delete`/
  `delete_object`/`lookup`/`member`/`lookup_element`/`lookup_elements`/`size`/`iter_objects`/
  `iter_objects_rev`/`keys`/`take`/`first_key`/`last_key`/`next_key`/`prev_key`/`memory`/
  `update_counter`/`update_element`). `insert` returns `Result<bool>` — **whether the object was
  retained** — so `Table::insert` reclaims the deep copy it made when the interior rejects it (`bag`'s
  duplicate is the only `false`). Generic defaults rebuild the object on the caller's heap; the hash
  interior overrides the element updates with destructive in-place writes. `iter_objects_rev()` is the
  **defaulted** reverse-navigation walk (the order an `ets:last/1` + `ets:prev/2` walk visits the
  objects): the default is the exact reverse of `iter_objects()` (the `ordered_set` semantics), and
  both hash interiors (`ChampInterior`/`HashInterior`) override it to return `iter_objects()`
  **unchanged**, because their `last`/`prev` **are** `first`/`next`. It is the single source of truth
  behind `Table::tab2list_rev` (and so `select_reverse`). Private — not part of the cross-crate API.

### `table/rename.rs` — `ets:rename/2` (oracle-pinned)
- `pub fn Table::rename(&mut self, name: Atom, is_taken: impl FnOnce(Atom) -> bool) ->
  Result<RenameOutcome>`, `pub enum RenameOutcome { Named { previous_name: Option<Atom> }, Unnamed }`.
  Only the recorded name changes, so the objects, `TableId`, owner, access, kind and keypos survive by
  construction. Oracle rules: with `named_table`, an already-registered new name (including its own)
  is `badarg` with nothing changed (the caller's `is_taken` predicate reports registry hits) and the
  BIF returns the **new name atom**; **without** `named_table` the rename always succeeds — the
  predicate is never consulted, no conflict is possible, and the BIF returns the **unchanged table
  identifier**. The crate keeps the primitive; the registry rekey, the write gate, the non-atom-name
  and unknown-table `badarg`s are the host's (`ETS_SURFACE.md` §1).

### `table/shared.rs` + `table/view/**` — the shared handle and its guard-scoped views
- `pub struct SharedTable` (`Arc<RwLock<Table>>`): `new`, `id`, `ref_count`, `read() -> Result<TableRead<'_>>`,
  `write() -> Result<TableWrite<'_>>`, `with_read(|v: &mut TableRead<'_>| …)`, `with_write(|v: &mut
  TableWrite<'_>| …)`, `try_into_inner -> Result<Option<Table>>`, `Clone`, `Debug`. A poisoned lock
  yields the **typed** `unsupported_ets: table lock poisoned` (never a panic, never a wrong result).
- The read/write surface is a **lifetime-tagged, guard-scoped view**, so a term read through the shared
  handle cannot outlive its guard (a compile error, pinned by `compile_fail` doctests):
  - `pub struct TableTerm<'g>` — a **private** `Eterm` + `PhantomData<&'g Table>`, with **no** method
    returning a bare `Eterm`. Accessors: `is_integer/is_float/is_atom/is_tuple/is_list/is_bitstring/
    is_binary/is_map/is_pid/is_port/is_reference/is_function`, `as_small_int() -> Option<i64>`,
    `as_atom() -> Option<Atom>`, `as_float() -> Option<f64>`, `tuple_arity()`, `tuple_element(i)`,
    `tuple_elements() -> Vec<TableTerm<'g>>`, `list_to_terms()`, `size()`, `eq_term(Eterm, atoms) -> bool`, `cmp_term(Eterm, atoms) -> Ordering`,
    and the copy-out `copy_into(&mut TermStore) -> Result<Eterm>` (deep copy into a caller-owned arena,
    the only way to keep a term past the guard; costs one allocation per call, opt-in).
  - `TableRead<'g>` — owned metadata (`id/size/is_empty/memory/info/kind/keypos/access/owner/name/
    named_table/is_fixed`) plus term reads returning `TableTerm<'g>` (`member`, `lookup`,
    `lookup_element`, `lookup_element_or`, `lookup_element_all`, `lookup_element_all_or`, `first`,
    `last`, `next`, `prev`, `tab2list`), and the **copy-out** select family (`select`, `select_count`,
    `match_object`, `match_` take a `&mut TermStore` destination and return owned terms, because a
    `'$_'`/variable body returns the stored object itself; internal scratch `Heap` is grown and retried
    on heap exhaustion).
  - `TableWrite<'g>` — metadata, mutations over caller-owned inputs (`insert`, `insert_new`,
    `insert_list`, `delete`, `delete_object`, `delete_all_objects`, `update_counter`,
    `update_counter_with`, `update_element`, `select_delete`, `match_delete`, `select_replace`,
    `set_fixation`, `set_owner`, `set_heir`, `set_access`, `setopts`, `give_away`), `heir()`, `take ->
    Vec<TableTerm<'_>>`, and `as_requester`.
  - **View-borrowed traversal** (`view/iter.rs`) — `TableRead::{iter_keys, iter_keys_rev, iter_objects,
    iter_objects_rev}` (each yields `TableTerm<'a>` bound to the view borrow via `ets:first|next` /
    `last|prev`; the `*_objects` forms expand the multi kinds per object), `TableRead::{foldl, foldr}`
    over objects, and the bulk copy-out `TableRead::tab2list_into(&mut TermStore) -> Result<Vec<Eterm>>`.
    The `*_rev` forms and `foldr` drive `ets:last/1` + `ets:prev/2`, so for an `ordered_set` they are the
    exact mirror of the forward forms and for the hash kinds they are the **same** traversal (their
    `last`/`prev` **are** `first`/`next`, `erl_db_hash.c:853-856`). Every item is a `TableTerm<'_>` tied
    to the borrow and the iterators are returned as `impl Iterator`, so no private view type leaks into a
    public signature. A full hash-kind walk is O(n²), so `tab2list_into` stays the bulk path.
  - The views are `!Send` by design (they carry a scratch `Heap`): neither a view nor its aliasing
    terms can move to another thread.

### `table/ownership.rs` — `ets:give_away/3` + `{heir, …}` + `{protection, …}` + the heir data
- `pub struct Heir { pub owner: OwnerId, pub data: Option<Eterm> }` — `Some(term)` = `{heir, Pid, Data}`
  (arity 3), `None` = `{heir, Pid}` (arity 2, the C `THE_NON_VALUE`: the heir inherits **silently**,
  with no `{'ETS-TRANSFER'}` message). `pub enum SetOpt { Heir(Heir), ClearHeir, Access(Access) }` — the
  parsed `ets:setopts/2` option (the subset modelled). `Table::setopts(caller, SetOpt) -> Result<()>`
  applies the **owner gate first** (`caller != owner` ⇒ `badarg`, nothing mutated), then `Heir`/`ClearHeir`
  (through `set_heir`/`replace_heir`) or `Access(a)` (through `set_access`).
- `Table::install_heir(owner, data: Option<Eterm>) -> Result<()>` — **copies** the gift data into
  table-owned storage (immediates by value; a non-copyable term is the typed `badarg` and the heir is
  left untouched). `Table::heir_data() -> Option<Eterm>` reads the table-owned copy back.
  `Table::copy_heir(store, heir)` is the shared `pub(super)` copy seam used both by `Table::build`
  (**a creation-time `{heir, …}` is deep-copied at build**, not stored verbatim) and by `install_heir`.
  `Table::replace_heir(heir)` is the shared `pub(super)` reclaim seam every overwrite site
  (`set_heir`, `setopts`, `install_heir`, `build`) funnels through, so the outgoing heir's copy is
  freed exactly once (the C `free_heir_data`). The non-copying primitives `set_heir`/`setopts` stay
  owner-level and documented as such.
- **`Table::set_access(&mut self, access: Access)` is the post-creation protection setter** (the
  owner-level primitive behind `setopts(_, SetOpt::Access(_))`). `Table` owns the live `access: Access`
  field (initialized from `opts.access()` in `build_with`); `access()`, `info().access`, `check_read`
  and `check_write` all read **that field**, so a runtime change is observable everywhere at once.
  `TableAccess::set_access(&mut self, access: Access) -> Result<()>` is the enforcing form (it routes
  through `setopts(self.requester, SetOpt::Access(access))`, so a non-owner is `badarg` and nothing
  changes); `TableWrite::set_access` is the guard-scoped mirror. **`TableOptions` is NEVER mutated**:
  the oracle does not re-normalize the fine-locking flags on a runtime protection change (a table
  created `[public, {write_concurrency, true}]` then `setopts({protection, private})` still reports
  `write_concurrency => true`, whereas a table created `private` reports `false`) — the creation-time
  normalization stays a `TableOptions`/`beam_bifs` concern (`ETS_SURFACE.md` §1/§5).
- `give_away` follows the C `ets_give_away_3` order (bad-pid → not-owner → self-gift, all `badarg`,
  owner unchanged on failure); `setopts` requires the caller to be the owner. The heir is stored as a
  full `Heir`; `ets:info` renders only the pid (a documented gap).
- **The pid gap.** This crate has no pid term and no process-liveness concept: a `None` new owner
  stands in for a non-pid/dead pid target, an unknown table id is the registry/BIF layer's concern, and
  a set heir/owner renders as `undefined`.

### `table/lifecycle.rs` — the owner-death transition
- `pub enum OwnerDeath { NotOwner, Deleted, Transferred(Heir) }` + `Table::owner_died(dead, is_alive)`
  — what happens when a process that (may) own the table dies. `NotOwner` is a complete no-op;
  `Deleted` (no heir, a self-heir, or a heir the liveness oracle rejects) obliges the runtime to drop
  the table; `Transferred(heir)` sets the owner to `heir.owner` and **keeps the heir installed** (the
  oracle does not reset it) and obliges the runtime to deliver
  `{'ETS-TRANSFER', Tid, dead_pid, data}` — **only when `heir.data` is `Some(data)`** — with `Tid` the
  registered name for a `named_table`, else the table id. Neither effect is this crate's.

### `table/access.rs` — the access-enforcing view
- `pub struct TableAccess<'a>` (from `Table::as_requester`) — rechecks `can_read`/`can_write` on every
  op and raises `BeamError::BadArg` on a violation. Wraps the whole observable surface: write-checked
  `insert`/`insert_list`/`insert_new`/`delete`/`delete_object`/`delete_all_objects`/`take`/
  `update_counter`/`update_counter_with`/`update_element`/`select_delete`/`match_delete`/
  `select_replace`/`rename`; read-checked `lookup`/`member`/`lookup_element(+_or/_all/_all_or)`/`select`/
  `select_reverse`(+_limit/_continue)/`select_count`/`match_object`/`match_`/`first`/`last`/`next`/`prev`/
  `tab2list`/`set_fixation` (the oracle gates `safe_fixtable/2` as a READ) and the `/3`+`/1` limited
  forms. `ets:match_spec_run/2` is a free function over a list with no table/requester, so it has no
  access concept. `ets:info/1,2` is deliberately **not** gated — `size`/`is_empty` are its ungated
  projection (the oracle resolves `ets:info(T, size)` with no read-status mask, so a non-owner of a
  `private` table reads the size while every read *and* write is `badarg`). `give_away(new_owner)`,
  `setopts(opt)` and `set_access(access)` are requester-implicit — `give_away` has no protection check
  (ownership is not a read/write op), while `setopts`/`set_access` are owner-gated (through
  `SetOpt::Access`), like the oracle.

### `table/update.rs` — `update_counter`/`update_element` operand machinery
- `pub(crate)` helpers shared with the hash interior: `counter_slot`, `add_counters`,
  `replace_elements` (re-exported from `table/mod.rs`); internals `object_arity`, `read_counter`,
  `counter_operand`, `counter_operand_error`, `warp_fires`, `decode_counter_ops`,
  `decode_counter_tuple`, `build_list`, `CounterOp`.

### `champ_interior.rs` — CHAMP open-addressing interior (**default `set`**)
- `pub struct ChampInterior` + `Default`/`new()`: each level consumes `WIDTH = 5` bits of the 64-bit
  `term_hash`, compressing children into a `datamap` (inline slots) + `nodemap` (sub-nodes); a full-hash
  collision lives in a `CollisionNode` (key-compared with `eq`), so a pathological hash cannot recurse
  past 64 bits. Every `Interior` method is overridden; `set` navigation order is unspecified, so
  `first_key`/`next_key` fall back to the trait defaults while `last_key(atoms) = first_key(atoms)`,
  `prev_key(k, atoms) = next_key(k, atoms)` and `iter_objects_rev() = iter_objects()` are the hash-kind
  aliasing (`erl_db_hash.c:853-856`) — the reverse walk **is** the forward one.
- **Delete canonicalization.** A removal canonicalizes bottom-up: a sub-node holding exactly one entry
  is **inlined** into its parent as an inline slot (promoting a multi-entry sub-node wholesale would
  break routing — a sub-node is indexed by the *next* 5 hash bits).
- `SetInterior` selects this (default) or `Chained`; `Table::with_set_interior` installs it.

### `hash_interior.rs` — chained hash-index interior (`bag`/`duplicate_bag`; opt-in `set`)
- `pub struct HashInterior`: buckets `Vec<Vec<Entry>>` (`Entry { key, objs: Vec<Eterm> }`), lookup by
  `term_hash(key) % buckets` then `eq`; doubles + rehashes past a load factor. `set` keeps ≤ 1 object
  per key; `bag` rejects a duplicate *object* within a key (its `insert` returns `false`, so the caller
  reclaims the rejected copy); `duplicate_bag` keeps a multiset. `delete_object` removes **every** equal
  object (the oracle). Public API for testing/reuse: `new(kind)`, `take(key, atoms)`,
  `update_counter(key, pos, incr, atoms)`, `update_element(key, updates, atoms)`, `memory(atoms)`. It
  implements `keys()` (with `first_key`/`next_key` from the trait defaults) and overrides the hash-kind
  navigation aliasing: `last_key(atoms) = first_key(atoms)`, `prev_key(k, atoms) = next_key(k, atoms)`
  and `iter_objects_rev() = iter_objects()` (`erl_db_hash.c:853-856`).

### `ordered_interior/` — the ordered `ordered_set` interiors (B+-tree + sorted `Vec`)
- **A same-arith-key replace replaces the stored KEY term too** (the key follows the object): an insert
  whose key is `cmp_arith`-equal to a stored key but a different term (`1` vs `1.0`) overwrites the
  stored entry's `key` along with its `obj`, so every navigation/iteration observes the last-inserted
  term — the oracle's behaviour (probed: `{1,a}` then `{1.0,b}` ⇒ `first =:= last =:= 1.0`). Safe
  because the two terms are `cmp_arith`-equal, so routing/separator comparisons are unchanged, and it
  keeps each stored key pointing into the current object's storage. `insert_new` never replaces.
- `mod.rs` — `OrderedSetInterior` (K-ary B+-tree): `new()`, `first`/`last`, `first_key`/`last_key`,
  `next`/`prev`, `next_from(bound, inclusive)`/`prev_from(bound, inclusive)`, all via
  `beam_term::compare::cmp_arith` (the arithmetic key order — `erl_db_tree.c` `cmp_key_eq`,
  `CMP = erts_cmp(a, b, 0, 0)`), so `1` and `1.0` are the same key. Whole-object identity
  (`delete_object/2`) stays the exact `compare::eq`. Plus its `Interior` impl; the node arena **is**
  counted by `memory()`.
- `sorted.rs` — `SortedOrderedInterior`: the same observable semantics over one sorted `Vec` (O(log n)
  lookup via the shared `node::search_entries`, O(n) insert/remove), same navigation, capacity-based
  `memory()`; selectable via `OrderedInterior::Sorted`.
- `node.rs` — the arena node layout, `ORDER`/`MIN_OCCUPANCY`, the `NodeStore` (`NodeId` slots + free
  list; nodes never relocate), the shared `Entry` record, `search_entries`, `words_of`.
- `tree.rs` — insert/lookup/delete with split + borrow-or-merge rebalance, the replace-key-term rule,
  the separator invariant `keys[i] == max(child[i])`.
- `iter.rs` — the leaf-chain ordered iterator.

### `select/` — the match-spec compiler + executor
- `mod.rs` — `pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;`
  `pub struct MatchSpec { pub clauses: Vec<MatchClause> }`, `pub struct MatchClause { head, guard, body }`,
  `pub fn compile(spec, atoms) -> Result<MatchSpec>`; aliases `MatchPattern`/`MatchElement` = `Pattern`,
  `MatchGuard`/`MatchBody`/`MatchBodyElement` = `Expr`.
- `pattern.rs` — `Pattern` (nested tuple/list/improper patterns, quoted literals, `'_'`, `'$N'` binds
  with repeat-occurrence equality; every other atom — including `'$_'`/`'$$'`, special only in a *body*
  — is an exact `=:=` literal).
- `expr.rs` — `Expr`/`CmpOp`/`TypeTest`/`ArithOp`: the guard/body language. Guards: `andalso`/`and`,
  `orelse`/`or`, `not`, `const`, bare truthiness; comparisons `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`
  (structural `cmp` for exact, `cmp_arith` for `==`-family); type tests `is_atom/integer/float/number/
  tuple/list/binary/bitstring/map/pid/port/reference/function/boolean`; guard BIFs `element/2`, `abs/1`
  (int, float or bignum — `abs(MIN_SMALL)` materialises the bignum `2^59`), `size/1`, `hd/1`, `tl/1`,
  `length/1`, `map_get/2`, `is_map_key/2`, `map_size/1`, `tuple_size/1`, `byte_size/1`, `bit_size/1`,
  `float/1`, `trunc/1`, `round/1`, `ceil/1`, `floor/1` (an integral result outside the small range is
  materialised as the oracle's **bignum**, from the float's bits), `bnot/1` (a bignum operand returns
  `-N-1`), and `self/0` (a typed `badarg` — see Known Issues). Map **construction** is supported in guards and bodies.
  Map/tuple/binary operands are read through `beam_term` primitives (`map::map_get`/`map_size`, …),
  never a local reader. **Arithmetic** (`+ - * / div rem band bor bxor bsl bsr`, unary `+`/`-`) routes
  through the `pub trait ArithHook` seam (default methods) with `DefaultArithHook` installed via a
  `thread_local` (`set_arith_hook`) — no crate edge to `beam_bifs`. The default does checked i64
  arithmetic and **faithful float arithmetic** (Erlang semantics): `+`/`-`/`*` yield a float when either
  operand is a float, `/` is **always** a float, unary `+`/`-` on a float yields a float, and a
  non-finite result ⇒ `badarg`; integer-only operators and a result that leaves the 60-bit small range
  (incl. a bignum operand) stay the typed `unsupported`; `div`/`rem`/`/` by zero ⇒ `badarg`; a negative
  shift count reverses direction; `bsr` saturates — the C `erts_shift`. Bodies: sequences
  (value = last expression), tuple/list/**map** construction, literals, `'$N'`, `'$_'`, `'$$'`. Guard
  truth is an internal `Expr::Bool` (independent of whether `true`/`false` are interned). Anything
  outside the recognised set ⇒ `compile`/`expr` `badarg`.
- `exec.rs` — `impl Table`'s select family + `pub enum BodyKind` (`WholeObject`/`AllBindings`/
  `LiteralTrue`) that reuses the compiler for `ets:match*`. `select_replace` runs the C's **static** body
  check (`static_keeps_key`/`clause_keeps_key`, mirroring `db_match_keeps_key`): a body that is not
  `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}` is whole-call `badarg` even when nothing matches;
  supported on set/ordered_set/duplicate_bag (`bag` ⇒ `badarg`). A BODY runtime
  failure is replaced by the atom `EXIT` and evaluation continues (the C `FAIL_TERM`). `guards_hold`
  returns `Result<bool>`: a guard the oracle would itself raise (`BadArg`) or a non-`true`/non-boolean
  guard skips the clause, while a **capability gap** (e.g. the `ArithHook`'s `unsupported_ets`)
  propagates as a typed error — an unevaluable guard is never silently a non-match.
- `run.rs` — `match_spec_run(list, spec, atoms, heap)`.
- `continuation.rs` — `pub struct MatchContinuation` (opaque: table id + the table's **registered-name
  atom** for a `named_table` (else `None`) + the compiled spec (body included) + limit + direction + a
  private `Cursor`) and the limited forms, each `-> (Vec<Eterm>, Option<MatchContinuation>)`. `Cursor` is
  `Begin` (a `/3` start) / `Anchor(key)` (an `ordered_set` resume: the last key the chunk yielded, used as
  an **exclusive** `next`/`prev` range bound, so it survives the anchor's own deletion) /
  `Position(ordinal)` (a hash kind's resume ordinal into the iteration order — ERTS's cursor there is a
  bucket/index, not a key, `erl_db_hash.c:2310-2313`). `resume` rejects a
  continuation whose table id **or** recorded name does not match (a `named_table`'s continuations die
  on `rename`, exactly like the oracle), and it re-applies the stored direction and spec so a `/1` resume
  needs no direction/spec argument.

### `info.rs` — introspection (`ets:info`)
- `pub struct TableInfo { id, name: Option<Atom>, kind, keypos, access, size, memory, owner:
  Option<OwnerId>, heir: Option<OwnerId>, named_table: bool, compressed: bool, write_concurrency: bool,
  read_concurrency: bool, decentralized_counters: bool }`;
  `entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)>` — the heap-free `{Key, Value}` pairs of
  the oracle's **15** `ets:info/1` fields in the oracle's order (`id`, `decentralized_counters`,
  `read_concurrency`, `write_concurrency`, `compressed`, `memory`, `owner`, `heir`, `name`, `size`,
  `node`, `named_table`, `type`, `keypos`, `protection`). `heir` is `none` when unset and `undefined`
  when set; `node` is the runtime-shape constant `nonode@nohost`; `decentralized_counters` is the
  resolved field (see `TableOptions::decentralized_counters`); `owner` renders `undefined` and `id` a
  small integer. The sibling `beam_bifs` bridge builds the observable proplist (same 15-field order).

### `error.rs` — failure constructors
- `pub(crate) fn badarg() -> BeamError`; `pub(crate) fn unsupported(what: &str) -> BeamError` (a
  `BeamError::Raise`, error class, reason atom `unsupported_ets: <what>` — currently the scalar
  `lookup_element` on a multi kind and an inaccessible table). No crate-local error enum.

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`. `badarg`-style failures ⇒ `BeamError::BadArg`;
  never `panic!`/`unwrap()` on runtime data. Table ops never panic on malformed input.
- **Never re-implement term order/hash** (`beam_term::compare::{cmp,cmp_arith,eq}`,
  `beam_term::hashing::term_hash`) — workspace Prevent-Duplication rule.
- **Table-owned storage only (A.1 copy boundary).** An inserted object is **deep-copied** into the
  table's `TermStore`; the table never retains a pointer into a caller's heap. Objects/keys returned
  from reads alias table storage and are valid for the table's lifetime (see Known Issues).
- **`unsafe` confinement.** `unsafe` only in `store/`, the tuple accessor in `key.rs`, and the small
  term-reading blocks in `select/` and `table/mod.rs` (`select_all`) — plus the `unsafe impl Send/Sync`
  on `TermStore`. Every `unsafe fn` has a `# Safety` section and every `unsafe` block a preceding
  `// SAFETY:` rationale (`beam_common::safety`).
- **Files ~1000 lines**, one responsibility per file. Every `src/` file is under it except
  `src/hash_interior.rs` (~1074), one cohesive chained-index interior (buckets + the `Interior` impl +
  the multi-kind object rules), kept whole rather than split. Large `tests/` modules are legitimate: the
  embedded oracle program `tests/differential/erl_program.rs` (one Erlang source) and the comprehensive
  `tests/select.rs`; the split-out unit-test siblings are `src/table/tests.rs`,
  `src/ordered_interior/tests.rs`, `src/store/tests.rs`, `src/kind/tests.rs`.
- **Licensing** Apache-2.0; no new external deps (only `beam_common`/`beam_term`/`beam_alloc`/`libc`).

## Design Decisions
- **The shared handle is guard-scoped, not copy-on-read.** `SharedTable` once handed out `RwLock`
  guards over the raw `Table`, so a term read under one guard and used after it was dropped could be
  reclaimed (its word runs go to the per-size free lists) and silently read back as a *different,
  well-formed* object. The fix is a lifetime-tagged view: `TableRead`/`TableWrite` hold the guard and
  every term they yield is a `TableTerm<'g>` bound to the view's borrow, which cannot escape the
  closure-scoped view — a **compile error** (two `compile_fail` doctests), not a documentation caveat.
  Cost on the default path is **zero** (one `Eterm` + a ZST); allocation happens only when a caller
  asks for it via `TableTerm::copy_into`/the copy-out `select` forms (which reuse the table's own
  deep-copier, so their supported subset is exactly what an insert can store). The views are `!Send` on
  purpose. `tests/ets_shared.rs` proves the aliasing is caught: `a_stale_raw_term_reads_as_a_different_
  object_after_arena_churn` shows the hazard through the raw API, and
  `copy_out_survives_guard_drop_and_arena_churn` shows the tagged path is immune. The raw `Table` /
  `TableAccess` read family still returns bare `Eterm`s — a cross-crate API decision (hosts consume it);
  `ETS_SURFACE.md` §7.6 lists the exact names. (Structural change → `../../CHANGE.md`.)
- **Heir gift data is table-owned; arity 2 is representable.** `Heir.data: Option<Eterm>` distinguishes
  the oracle's `{heir, Pid}` (arity 2, transfers silently) from `{heir, Pid, undefined}` (arity 3,
  transfers `undefined`). The data is deep-copied into the table store at creation (`Table::build` via
  `copy_heir`) and at `install_heir`, and reclaimed by `replace_heir` at every overwrite site — the C
  `ERTS_ALC_T_DB_HEIR_DATA`/`free_heir_data` pair. Storing the caller's term verbatim (the old
  behaviour) would have dangled once the installing process's heap died.
- **`ordered_set` defaults to a K-ary B+-tree** (`ORDER = 32`, `MIN_OCCUPANCY = 16`) with an
  arena-backed, non-relocating `NodeStore` (`NodeId` + free list), O(log n) insert/lookup/delete and a
  leaf chain; keys compare by `cmp_arith`. The sorted-`Vec` interior is retained as a selectable
  comparison index (`OrderedInterior::Sorted`) — measured: at n = 1e5 the sorted `Vec` is 3.9–6.3×
  slower inserting and 3.1–34.6× slower deleting (O(n) tail shift) against parity on lookup, ~0.1–0.45×
  on ordered iteration and 0.73× on memory, so it can never be the default for a table whose defining
  operations are insert/lookup/delete. `src/interior_bench.rs` reproduces the numbers; the interior
  shape is unpinned. (Structural change → `../../CHANGE.md`.)
- **`set` defaults to CHAMP; `bag`/`duplicate_bag` stay chained.** Measurement-driven:
  `src/interior_bench.rs` times the index only across 4 key shapes × 4 sizes; CHAMP insert 0.08–1.07×,
  iteration 0.20–1.48×, memory 0.14–0.75× of chained (2–7× fewer index bytes/entry), lookup-hit
  0.20–1.50×, but delete 0.69–2.77× (1.6–2.8× slower — canonicalization) and small-int lookup-miss up
  to ~2.1×. The insert/iterate/memory profile ETS is optimised for wins, so `SetInterior::default() ==
  Champ`; `Chained` stays selectable for `set` and is still the index for the multi kinds.
  `src/interior_parity.rs` drives both through one script and proves the observable traces identical.
  (Structural change → `../../CHANGE.md`.)
- **Interiors are a private `Interior` trait, not a pinned layout.** Generic defaults
  (rebuild-on-heap) with hash overrides (in-place destructive element writes) so the B+-tree needs no
  writable index slot.
- **One `TermStore` per table; copy-on-insert, per-object free.** `free(root)` returns a removed root's
  exact-word-size runs to per-size free lists that `copy` reuses before bumping; it is a no-op for a
  root the store never tracked (which is what makes the heir reclaim path safe).
- **`take` returns checked-out copies and reclaims the stored originals.** Copy-before-free is
  mandatory (freeing first would let a later same-size `copy` reuse the words the caller holds), and
  each returned copy is `relinquish`ed: valid for the table's lifetime, never handed out again, no
  longer counted toward `memory()`.
- **Access control mirrors `db_check_access`.** `can_read`/`can_write` treat `requester == owner` as
  full access; `TableAccess` is the enforcing view while the raw `Table` ops stay owner-level
  primitives.
- **`ets:info/1` mirrors the oracle's 15 fields in the oracle's order** (the reverse of the C
  `fields[]` array in `erl_db.c:4406`); `heir`/`node` are runtime-shape constants,
  `decentralized_counters` is the resolved value: effective `write_concurrency` **and** the explicit
  `{decentralized_counters, Bool}` when given, else `kind.is_ordered()` (the CATREE default). `private`
  normalizes the concurrency flags off (the `is_fine_locked && !DB_PRIVATE` rule), which makes
  `decentralized_counters` false too.
- **Ownership/heir semantics are a `Table`-level concern, not an access-mode one.** `give_away/3` and
  `setopts/2` do not consult `protection`; they are owner-gated. The heir is stored as a full `Heir`
  for the runtime's `{'ETS-TRANSFER',…}` message, but `ets:info` renders only the pid.
- **`insert_list` is atomic; `select_replace/2` is a two-phase rewrite with the C's static body check**
  (a body outside `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}` aborts with `badarg` — even with
  no match — leaving the table unchanged; a valid body is applied atomically).
- **A `duplicate_bag` replacement rebuilds the key and reclaims the objects it replaces.** Several
  objects share a key and the replacement must land on the **specific** matched object, so the affected
  key's whole object list is rebuilt: the current objects are read through `Table::lookup` (they alias
  live table storage), each replacement is computed on the caller's scratch heap, and
  `Table::replace_key` deep-copies the new list into table storage **first** and only then removes the
  old objects through the ordinary freeing path (`delete`). Copying before freeing is what guarantees no
  replacement reads reclaimed storage, and the ordinary freeing path is what keeps the store's
  free-run reuse at a steady state — a rebuild from `Table::take`'s checked-out copies (whose storage
  `relinquish` holds for the table's lifetime) would map a fresh block per round instead. A
  replacement that equals the object it replaces is harmless because the rebuild never removes an
  object *by value*, which `delete_object`'s "remove **every** equal object" rule would. Pinned by
  `table::tests::a_duplicate_bag_replacement_reclaims_the_replaced_objects` (the store's block count
  and live words stay at their warmup floor across 100 rounds) and the oracle-visible
  `ets_replace::repeated_identical_replacement_on_a_duplicate_bag_is_stable`.
- **Match-spec arithmetic routes through the `ArithHook` seam** (a `thread_local`), keeping wave-1
  `beam_ets` free of a `beam_bifs` dependency; candidate to lift into `beam_term` later.
- **Continuations are opaque owned values, not terms** (table id + compiled spec + limit + position),
  so `select_continue` resumes without re-compiling; `Option::None` is the crate's `'$end_of_table'`.
- **A BODY runtime failure yields the atom `EXIT`; a GUARD failure skips the clause** (verified against
  `erl`: `[{{'_','_'},[],[{hd,5}]}]` ⇒ `['EXIT','EXIT']`).
- **Guard truth is an internal boolean** (`Expr::Bool`), so it never depends on the atom table;
  boolean *bodies* still render as the interned atoms `true`/`false`.
- **`match_delete` returns the deleted count**, not the BIF's `true` (the count is the only extra
  information the operation can report).
- **`&AtomTable` is passed per call** — a `Table` cannot own atom state (indices are global).
- **`size` counts objects, not keys** (matching `ets:info(T, size)`).
- **The registered `name` and the `named_table` flag are independent** (`set_name` records the name only).
- **Hash-kind navigation order is unspecified (a deliberate freedom).** Every key a walk yields is
  present, `first`+`next` is a permutation of the stored keys, and `next`/`prev` on an absent anchor are
  `badarg`; `ordered_set` navigation is exact (ascending arithmetic key order; absent keys read as
  bounds).
- **An interior's stored key term aliases a live, table-owned object.** `key_of(object, keypos)` returns
  a pointer *into* the object's table-owned `TermStore` storage, so an interior that keeps a key must
  re-point it whenever it replaces or removes the object it aliased: on a `set` upsert
  (`hash_interior`/`champ_interior`) and, for `bag`/`duplicate_bag`, when `delete_object` removes the
  object whose key element the entry's stored key pointed at (re-derived from a surviving object at
  `keypos`). `Table::relocate_rebuilt_object` derives the index key from the **table-owned copy**, never
  the caller's key. This was the crate's live use-after-free class — see `tests/ets_reclamation.rs`.
- **B+-tree branch separators are re-materialised from the rightmost live leaf, but only where the
  subtree max actually moves.** `Tree::insert_rec` returns an `InsertStep` that carries the ascending
  path's new maximum, so on the unwind a separator is rewritten only when the subtree maximum it names
  changed (or a split creates a new one) — an insert under an unchanged maximum touches no separator.
  A separator that IS replaced must be re-materialised (`NodeStore::subtree_max_key`): an upsert
  replaces the object that established the subtree maximum and reclaims its storage, so a stored
  separator term would dangle even when the key *value* is unchanged. The branch descent is a lower
  bound, so `node::child_index` **binary-searches** the strictly ascending separators (`O(log ORDER)`,
  ≤5 `cmp_arith` at `ORDER = 32`) instead of scanning them. `Tree::check_invariants` asserts separator
  **term identity**; `an_upsert_that_moves_a_subtree_maximum_leaves_every_separator_live` pins it.
  (Structural change → `../../CHANGE.md`.)
- **`insert_new` is a single descent.** The same `insert_rec` runs with a refuse-on-existing-key flag,
  stopping with `InsertStep::Refused` and mutating nothing, so `ets:insert_new/2` no longer performs a
  separate `find` before its `insert` (≈1.5–2.2× faster; `interior_bench.rs`).
- **`{write_concurrency,_}` is a mode, not a bool.** `WriteConcurrency` (`Off`/`On`/`Auto`/`FixedLocks`)
  is the model; the bool API is its `Off`/`On` projection, so `beam_bifs` stays source-compatible, and
  `decentralized_counters` is derived from the mode. The fine-locking normalization (power-of-two lock
  count, `>= NLOCKS_WITH_ITEM_COUNTERS`, the `private`-forces-off rule) is a **creation-time** property
  of `TableOptions`: a runtime `set_access` change does **not** re-normalize it (the oracle agrees — see
  the `table/ownership.rs` entry).
- **The guard-scoped view gains a view-borrowed traversal API** (`view/iter.rs`) returning `TableTerm`
  bound to the borrow — so a full table walk is expressible without a bare `Eterm` escaping.
- **The hash kinds' `last`/`prev` ARE their `first`/`next` (C-faithful navigation).** ERTS's hash
  interior installs the same function twice — `db_first_hash, db_next_hash, db_first_hash /* last ==
  first */, db_next_hash /* prev == next */` (`../../erts/emulator/beam/erl_db_hash.c:853-856`, repeated
  at `:891-894` for the `*_lookup` table) — and `db_select_reverse_hash` delegates to the same chunk
  function as `db_select_hash` (`:2310-2313`). So for `set`/`bag`/`duplicate_bag` there is **no**
  backward walk: `ets:last/1` == `ets:first/1`, `ets:prev/2` == `ets:next/2` (an unknown anchor stays
  `badarg` through `Table::check_navigable`), `'$end_of_table'` ends the walk, and `ets:select_reverse`
  returns exactly what `ets:select` returns. `ChampInterior`/`HashInterior` reproduce it by overriding
  `last_key`/`prev_key`/`iter_objects_rev`; the `Interior` trait's defaults stay the `ordered_set`
  semantics and `ordered_set` keeps the true arithmetic order (`first`/`last`, `next_from`/`prev_from`).
  Probe values (system `erl` 29.1.1 / erts-17.1): keys `1..5` ⇒ `first` == `last` == `1`,
  `next(T,3)` == `prev(T,3)` == `4`, `prev(T,1)` == `next(T,1)` == `5`; a 6-key set ⇒ `first` ==
  `last` == `6`; a single-key set ⇒ `{only,only,'$end_of_table','$end_of_table'}`; an empty hash-kind
  table ⇒ `first` == `last` == `'$end_of_table'` while `prev(T,7)` is `badarg`. Pinned by
  `multikind/hash_last_prev_alias_first_next` and `multikind/hash_select_reverse_aliases_select`.
  (Structural change → `../../CHANGE.md`.)
- **Select-family continuations are key-anchored on an `ordered_set`, ordinal on the hash kinds.**
  `MatchContinuation`'s private `Cursor::{Begin, Anchor(Eterm), Position(usize)}` is chosen by the table
  kind, which never changes after creation, so the two shapes are never mixed. The ordered kind resumes
  with `Table::next`/`prev` from the **last key its chunk yielded** (`Anchor`), an **exclusive** range
  bound, so the resume survives the anchor's own deletion (probed: `{1}..{10}` chunked at 4 ⇒
  `[1,2,3,4]`, delete `2` + insert `0` ⇒ `[5,6,7,8]` then `[9,10]`; deleting the anchor `4` instead ⇒ the
  identical `[5,6,7,8]`). The hash kinds keep the ordinal `Position` because their ERTS cursor is a
  bucket/index, not a key (`erl_db_hash.c:2310-2313`), and this crate documents the hash iteration order
  as an interior freedom. Continuation identity is unchanged (the table id + the `named_table` name atom;
  the read gate precedes consumption). (Structural change → `../../CHANGE.md`.)

## Concurrency, fixation & sharing (current state)
- **`Table` is `Send + Sync` and shareable across processes**: plain `Eterm` data + a
  `Box<dyn Interior>` (`Interior: Send + Sync`) + a `TermStore` (`unsafe impl Send/Sync` with a
  rationale: no interior mutability, every storage-reaching method takes `&mut self`).
- **`SharedTable`'s coarse lock is the whole story — no lost updates.** One `write()` guard spans a
  whole check-then-act sequence (`insert_new`, `update_counter`, `select_replace`), so concurrent
  writers lose nothing; fine-grained `{write_concurrency}` affects throughput only, never results. A
  poisoned lock is a typed error.
- **Read results are guard-scoped.** Through `SharedTable` a read yields a `TableTerm` bound to the
  view's borrow (see Design Decisions); a caller that needs the term later copies it out
  (`copy_into`/the copy-out select forms) under the guard. The raw `Table`/`TableAccess` read family
  still returns bare `Eterm`s (Known Issues).
- **`ets:safe_fixtable/2` fixation = deferred reclamation.** While fixed, every removed object's
  storage goes onto `deferred_frees` (and `delete_all_objects` defers each root instead of `reset()`ing);
  unfixing flushes them, so `ets:info(T, memory)` is held while fixed and falls at unfix.
  `deferred_frees` is not counted in `memory()`.
- **Subterm sharing (the copy-boundary trim).** `TermStore::copy_term` shares exactly the one literal
  whose liveness the crate can prove — the global empty tuple (`copy.c:130-140`);
  `beam_term::literal::LiteralArea` literals are deep-copied (the area may be dropped and the crate
  holds no handle). `{keypos, N}` keys live inside the stored tuple; `compressed` is parsed/reported
  with no storage effect.
- **Evidence:** `tests/ets_shared.rs` (Send/Sync bounds for `Table`, `SharedTable` **and** `TermStore`,
  the 8-writer no-lost-update case, reader consistency, the owner-only writer under concurrency, the
  cross-thread move, `two_interleaved_accessors_yield_the_serial_result` (two handles, two threads,
  lock-step behind a `Barrier`, exact serial outcome), `concurrent_counter_increments_lose_no_updates`
  (8×500 with the exact multiset of returned values), `readers_see_only_consistent_states_under_writer_
  churn` and `concurrent_counter_total_agrees_with_the_live_oracle`); `tests/ets_fixtable.rs` (5 cases).
  Plus `tests/ets_concurrency.rs` (7 cases): the coarse-lock scope (an exclusive write guard on one
  table does not block writers on other tables; a writer on a held table blocks until release), sharded
  serial-equivalence, lossless `insert_new`/`update_counter`/`select_replace` check-then-act under one
  guard, and 3 extra `compile_fail` doctests proving an iterator/term/accumulator cannot escape a view.

## Test Strategy
- Per-module `#[cfg(test)]` tests in every `src/` module (`store/tests.rs`, `kind/tests.rs`,
  `table/tests.rs`, `table/view/tests.rs`, `ordered_interior/tests.rs` hold the big ones;
  `interior_parity.rs`/`interior_bench.rs` are `#[cfg(test)]` siblings). Coverage:
  kind/options, key validation, store copy/free/reuse/relinquish, table insert/take accounting,
  hash-interior ops, CHAMP ops (round-trip, 2000-key churn, collision node, canonicalization,
  memory monotonicity), B+-tree invariants (incl. 10 000-key churn), the sorted-`Vec` interior, the
  mixed-class order test, select compile/execute, info, owner-death, the heir forms/reclaim, the
  duplicate_bag `select_replace` rebuild's storage floor (the store's `words`/`blocks` stay at their
  warmup floor across repeated replacement), and the guard-scoped views (incl. the aliasing defence and
  the `compile_fail` doctests).
- **Cross-interior equivalence + defaults** (`interior_parity.rs`): a shared LCG operation script drives
  chained vs CHAMP (identical traces, order-insensitive) and B+-tree vs sorted `Vec` (order-sensitive
  tail), pins the default selectors, and pins the `cmp`→`cmp_arith` arithmetic-key behaviour (the
  `{1,a}`/`{1.0,b}` collapse and the replaced key term).
- **Microbenchmarks** (`interior_bench.rs`): chained vs CHAMP and B+-tree vs sorted `Vec`, with in-line
  correctness assertions. Run with
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets --lib
  interior_microbenchmark -- --nocapture` (or `ordered_interior_microbenchmark`); sizes/shapes via
  `BEAM_ETS_BENCH_SIZES`/`BEAM_ETS_BENCH_SHAPES`.
- Crate-level integration over the **public API**: `tests/ets_{basic,order,select,bag,info,nav,access,
  update,replace,continuation,continuation_direction,set_access,ownership,fixtable,shared,options,rename,
  reclamation,concurrency,store_terms}.rs`, `tests/select.rs`, `tests/match_spec_run.rs`, sharing helpers
  from `tests/common/`, and the live-`erl` plumbing in `tests/common/oracle.rs`. `ets_store_terms.rs`
  cross-checks every newly-copyable boxed kind (hash map, each fun kind, external pid/port/ref, bitstring,
  refc binary) through `Table::insert`/`lookup` against the oracle's `phash2`. `ets_replace.rs` also
  pins that 100 repeats of the identical `duplicate_bag` replacement stay oracle-identical (`{ok:3,…}`
  and the same `tab2list`) with a flat `Table::memory`.
- **Differential evidence suite** (the compatibility gate): `tests/differential.rs` runs **159 cases**
  (13 case-group files under `tests/differential/battery/` plus the shared harness `battery/mod.rs`, the
  embedded oracle program in `erl_program.rs`) identically under the system `erl` (OTP 29.1.1 / erts-17.1)
  and under `beam_ets`, comparing canonical renderings line-by-line — **every case agrees**;
  `KNOWN_DIVERGENCES` is empty. The hash-kind navigation/reverse aliasing is pinned by the `multikind`
  group's `hash_last_prev_alias_first_next` and `hash_select_reverse_aliases_select`. The oracle is
  **required by default** — a genuinely absent
  `erl` FAILS the cross-check rather than skipping it (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out with a loud
  skip; `BEAM_ETS_REQUIRE_ORACLE=1` forces the requirement).
- **`tests/ets_continuation.rs` / `tests/ets_continuation_direction.rs` cross-check the continuation
  cursor against the live oracle, including a mutation between chunks**: an `ordered_set` resume
  survives a delete of the last-yielded (anchor) key, and a delete/insert behind the cursor does not
  shift the next chunk.
- **Gate (this crate):** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets`
  is green — **25 test binaries, 559 passed / 0 failed** (including the 8 doctests — 5 of them
  `compile_fail`); with the oracle required, the battery emits and compares all **159** cases
  (`[differential] oracle: … — 159 cases emitted` / `— comparing 159 cases against beam_ets`).
  Forced-oracle: `BEAM_ETS_REQUIRE_ORACLE=1 erts_rust/scripts/cargo.sh test
  --manifest-path erts_rust/Cargo.toml -p beam_ets --test differential`.

## Known Issues (honest gaps — not full parity)
- **The raw `Table`/`TableAccess` read family still returns bare `Eterm`s that alias table storage** —
  the same hazard class the shared handle now closes at compile time. A caller can hold such a term past
  its borrow (or past `take`, or past the table itself) and then observe words reclaimed and reused by
  a later same-size insert. The tagged `TableRead`/`TableWrite` views close this for terms, **with two
  exceptions**: `TableWrite::as_requester` (a documented residual hazard re-exposing the raw
  `TableAccess` bare-`Eterm` reads) and `TableWrite::heir`, whose `Copy` `Heir.data: Option<Eterm>` is a
  table-owned gift-data copy that a later `set_heir`/`replace_heir` frees and recycles, so the term
  escapes the guard's lifetime. The raw API fix is a cross-crate decision (hosts consume it) —
  `ETS_SURFACE.md` §7.6 names every method that should move behind the tagged-term discipline or a
  scoped `Table::view(&self)` handle.
- **`Concurrency` is advisory.** `read_concurrency`/`write_concurrency` are parsed and reported but
  there is no fine-grained locking; the only synchronisation is `SharedTable`'s coarse `Arc<RwLock>`.
  The `{write_concurrency, auto}` and `{debug_hash_fixed_number_of_locks, N}` modes **are** representable
  (`WriteConcurrency::Auto`/`FixedLocks(n)`, with `decentralized_counters` derived accordingly); what
  remains host-side is the *rendering* — `TableInfo::write_concurrency` is still a `bool`, so the `auto`
  atom / the normalized `{debug_hash_fixed_number_of_locks, Nlocks}` tuple is supplied by the
  `beam_bifs` `TableProfile` sidecar. The stored `FixedLocks` count is the requested, un-normalized one.
  **Owner: this crate** — fine-grained locking is a **throughput** workstream, never a correctness gap
  (one `write()` guard spans a whole check-then-act sequence, so no update is lost).
- **The owner-death transition's runtime caller reclaims named and unnamed tables alike.** `Table::owner_died`
  is driven by `beam_rt::runtime::ets_owner::drain_ets_owner_exits` (over the `EtsExitObserver` installed
  on the `ProcessTable`), and `beam_bifs`'s `ets:new/2` records the owner. The set of tables a dead owner
  owned is the `beam_bifs::TableRegistry::handles_owned_by` roster — **unnamed** tables included — with the
  `OwnerIndex` recorded at `ets:new/2` unioned in for the one thing the roster cannot supply, the
  **registered name** a `named_table`'s drop must release (`ETS_SURFACE.md` §7.4).
- **`TableInfo::entries` renders `owner` as `undefined` and `id` as a small integer.** The crate's own
  15-field proplist has no pid/ref term; the observable `ets:info/1,2` bridge in `beam_bifs` substitutes
  the recorded owner pid and the tid reference, so the BIF output matches the oracle. `heir` is `none`
  when unset and `undefined` when installed (the structured `TableInfo::heir` carries the pid).
- **Ownership modelling has no pid/liveness concept.** `give_away/3` treats `None` as the oracle's
  non-pid/dead-pid target (both `badarg`); an unknown table id is the registry/BIF layer's concern.
  `Table::owner_died` decides the transition and returns `OwnerDeath`; the delivery of the transfer
  message and the dropping of the table stay the runtime's job.
- **`ets:setopts/2` is modelled for `{heir, …}` and `{protection, Access}`.** `SetOpt` carries
  `Heir(Heir)`/`ClearHeir`/`Access(Access)`; other options (`safe_fixtable`, `{write_concurrency, _}`,
  …) are out of scope; `ets:info(T, holder)` is `badarg` in the oracle and none is emitted. The BIF
  bridge's `{protection, _}` wiring belongs to `beam_bifs`.
- **Scalar `lookup_element` is a deliberate typed refusal for the multi kinds.** `Table::lookup_element`/
  `lookup_element_or` raise the typed `unsupported_ets` for `bag`/`duplicate_bag` — a scalar `Eterm`
  cannot express the oracle's *list* — and the faithful forms are
  `lookup_element_all`/`lookup_element_all_or`. The `beam_bifs` BIFs already branch on
  `kind().is_multi()` to those forms (`beam_bifs/src/ets.rs::lookup_element_3/4`), so the refusal is
  unreachable through the BIF surface and is not an open gap.
- **`tab2file`/`file2tab`/`tabfile_info` are BLOCKED, not merely out of scope.** The table-file format
  is `{Header, Objects, Footer}` with every field an ETF term, so it needs `beam_codec` plus file I/O
  (`beam_io` is wave 5 — an upward edge, forbidden for a wave-1 crate). `tab2list` + `first`/`next`
  remain the enumeration path. Full reasoning in `ETS_SURFACE.md` §5.
- **Operations outside this crate's scope** (all present in OTP 29): `update_element/4`, the
  `foldl`/`foldr` **BIFs** (the view layer has `TableRead::{foldl,foldr}` traversal, but the BIFs need a
  `FunCaller`), `whereis/1`, `table/1,2`, `slot/2`, `tab2file*`/`file2tab*` —
  `ETS_SURFACE.md` §1–§5 names the owner and the blocker for every one. (`ets:clear/1` and `ets:size/1`
  do not exist in OTP 29, so their absence is correct.) The `/3` limited forms are implemented; their
  opaque continuation terms and the bare-vs-`{Results,'$end_of_table'}` shaping are `beam_bifs`'s.
  `select_reverse/1,2,3` is a crate primitive **and** is adopted by `beam_bifs::ets::reverse` (read-gated)
  over `Table::{select_reverse, select_reverse_limit, select_reverse_continue}`, sharing the
  direction-aware continuation dispatch with `ets:select/1`.
- **`match_spec_run` accepts a RAW spec, unlike the oracle.** The oracle requires
  `ets:match_spec_compile/1` first (a raw clause list is `badarg`); this crate has no compiled-spec
  *term*, so `run.rs` compiles the raw spec and the bridge maps `match_spec_compile/1` onto the compiled
  `MatchSpec`.
- **Named-table lookup-by-name is the runtime's job.** `Table` exposes `name()`/`is_named()` only; the
  name→table registry is the sibling `beam_bifs::ets::TableRegistry` (this crate holds no global state).
- **A hash-kind continuation is an ordinal into iteration order; an `ordered_set`'s is a key anchor.**
  The oracle's `ets:select/1` continuation carries the last key and resumes relative to it, so
  inserts/deletes do not shift what the next chunk returns. This crate reproduces that for `ordered_set`
  — `MatchContinuation`'s `Cursor::Anchor` is the last-yielded key, used as an **exclusive**
  `Table::next`/`prev` bound — oracle-verified across a mutation *including the anchor's own deletion*
  (`tests/ets_continuation.rs`/`tests/ets_continuation_direction.rs`). The three hash kinds keep the
  ordinal `Position`, because ERTS's cursor there is a bucket/index (`erl_db_hash.c:2310-2313`) and this
  crate's hash iteration order is an interior freedom; a mutation landing at or behind the ordinal can
  therefore skip or repeat an object relative to the oracle. That divergence only appears across a
  resume straddling a mutation — never a typed error and never a wrong result for a mutation-free walk
  (every single-chunk and no-mutation-between-chunks case agrees; the 159-case battery covers the
  latter).
- **Deep-copy covers every kind the oracle accepts in `ets:insert/2`.** Immediates, cons cells, tuples,
  flat **and hash** maps (HAMT/collision nodes copied node-by-node, no `AtomTable` needed), funs (all
  three kinds — header/descriptor verbatim + deep-copied env), boxed leaves (float, bignum, internal ref,
  on-heap bitstring), **external pid/port/reference** (raw data words, `node` slot deep-copied) and
  **sub-bitstrings / off-heap refc binaries** (re-materialised as self-contained on-heap bitstrings — the
  payload is gathered, no off-heap pointer survives) all copy into table storage. The residual typed gaps
  (`BadArg`, never a wrong result) are a bare `BinRef` root (never a standalone term), a native record
  (`RECORD_SUBTAG`), the reserved boxed subtags, a bare header word, and a malformed `ErlSubBits` (null
  base or `end < start`). Evidence: `src/store/tests.rs` (per-kind units + the accounting invariant) and
  `tests/ets_store_terms.rs` (live-oracle cross-check).
- **The match-spec compiler covers essentially the whole `guard_tab[]`.** The decoder
  (`src/select/expr/{mod,guard}.rs`) accepts the connectives, the comparisons, the type tests, the
  guard BIFs (`element/2`, `size/1`, `hd/1`, `tl/1`, `length/1`, `map_get/2`, `is_map_key/2`,
  `map_size/1`, `tuple_size/1`, `byte_size/1`, `bit_size/1`, `float/1`, `trunc/1`, `round/1`, `abs/1`)
  **and the wider forms** `ceil/1`, `floor/1`, `max/2`, `min/2`, `binary_part/2,3`, `is_record/3`,
  `is_function/2`, `is_integer/3`, `node/0,1`, `xor/2`, `bnot/1`, plus map **construction** in a guard
  or a body. Anything outside that set is `badarg`, like the oracle (`list_to_tuple/1` is not a
  `guard_tab` member and is `badarg` on both sides; there is no `is_record/2`, no comprehension and no
  `process_info`-style guard). Match-spec arithmetic via the default `ArithHook` is small-int/float only
  (checked `i64` plus faithful float `+ - * /`); full arithmetic needs a host-installed hook via
  `set_arith_hook`, whose owners are `beam_term` (the number/arithmetic layer, which exposes no big
  add/sub/compare LCA today) and `beam_bifs`/`beam_rt` (the installers) — no host installs one today, so
  it stays a typed gap that this crate never fills itself. Typed divergences (always a typed error,
  never a wrong result): a **head** map's **keys** are literal — a variable/underscore key is `badarg`
  at every nesting depth — while its **values** are matched structurally like tuple elements, so
  `#{a => '_'}` matches `#{a => 1}` and an object carrying extra keys still matches (a wildcard/variable
  inside a *nested head tuple used as a map value* is likewise matched structurally, not
  under-matched); `self/0` resolves through the thread-local `SelfHook` seam (`set_self_hook`) and its
  default is the typed `badarg` — the ETS layer has no process identity, so without an installed hook a
  `self/0` guard clause is skipped and a `self/0` body is `badarg` (the oracle evaluates it against the
  calling process: a semantic under-match); integer arithmetic whose result leaves the 60-bit small range
  (incl. a bignum operand) via `DefaultArithHook` is the typed `unsupported_ets` (float `+ - * /` **is**
  computed; `abs`/`bnot`/`trunc`/`round`/`ceil`/`floor` do handle bignums — see `select/CONTEXT.md`);
  `binary_part/2,3` on a non-byte-aligned source is `badarg`, as the oracle.
  `byte_size`/
  `bit_size` on an off-heap refc binary are **parity** (the `[BinRef][ErlSubBits]` root classifies as a
  bitstring). A match-spec **body** runtime failure yields the atom `EXIT` resolved from the caller's
  `AtomTable` (`select::exec::exit_term`), so a host running a raising body must pre-intern `EXIT`
  (real runtimes do); an uninterned `EXIT` ⇒ typed `badarg`.
- **`select_replace/2`'s key check is the C's static syntactic check** (`db_match_keeps_key`). A
  replacement clause whose body is not syntactically `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}`
  is `badarg` for the whole call, even when nothing matches, exactly like the oracle (pinned by the
  `static_check_*` tests and the `replace` battery). The check runs on the decoded AST
  (`select::exec::{static_keeps_key, clause_keeps_key}`); two exotic shapes remain approximated (a
  call-shaped raw tuple inside a `{const, X}` subterm compared against a literal head tuple, and the
  double-tuple unwrap inside a const subterm) — reachable only from hand-written specs, never a wrong
  result for any spec the battery or the oracle probes exercise.
- **`cmp_arith`'s big-integer↔float key comparison is a best-effort `f64` approximation** (a
  pre-existing `beam_term` gap). ERTS compares an integer key against a float key numerically;
  `number_as_f64` rounds an out-of-range bignum to `±inf`. Every representable integer/float pair
  (including the small-int/float keys the tests pin) is exact.

## Handoffs (to other nodes — NOT adopted here)
- **`beam_bifs` — adopt the new `{protection, Access}` setter.** This crate now exposes the post-creation
  protection change (`SetOpt::Access(Access)` + the owner-gated `Table::setopts(caller, SetOpt)` + the
  primitive `Table::set_access` + the enforcing `TableAccess::set_access`). `beam_bifs::ets::adopt::setopts_2`
  still answers the typed `unsupported_bif` for **any** `{protection, _}` (it validates the argument, then
  returns the gap before touching the table). The adoption is: after the existing owner check, map the
  parsed protection onto `SetOpt::Access` and call `table.as_requester(Some(caller)).setopts(opt)` — the
  parse-level `{protection, bogus}` `badarg` stays `beam_bifs`'s (`parse_setopts`).
- **`beam_bifs` — the `auto` / `{debug_hash_fixed_number_of_locks, Nlocks}` rendering stays host-side.**
  `TableInfo::write_concurrency` is a `bool`, so the `TableProfile` sidecar must supply the `auto` atom /
  the normalized lock count (`TableOptions` stores the *requested*, un-normalized count).
- **`beam_bifs` — the hash-kind `last`/`prev` workaround can now go.** `beam_bifs/src/ets.rs` still
  special-cases the hash kinds: `last_1` at `:1141-1151` (the branch at `:1144-1148` —
  `if table.kind().is_ordered() { table.last(..) } else { table.first(..) }`) and `prev_2` at
  `:1182-1197` (the branch at `:1190-1194` — the same shape over `prev`/`next`), i.e. the `:1144-1195`
  span. `Table::last`/`Table::prev` are C-faithful now (the interior aliases them to `first`/`next` for
  the hash kinds, `erl_db_hash.c:853-856`), so both BIFs could call `table.last(..)`/`table.prev(..)`
  unconditionally and drop the branch (the ordered-kind `'$end_of_table'` anchor rejection stays).
- **`beam_bifs`/`beam_rt` — install an `ArithHook` for full match-spec arithmetic.** `set_arith_hook`
  has no production caller, so the default (`DefaultArithHook`) stays small-int/float only and a bignum
  operand or an out-of-range integer result is the typed `unsupported_ets`. A runtime installs
  `beam_ets::select::expr::ArithHook` per thread over `beam_term`'s number layer to lift that gap.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, `SetInterior`/`OrderedInterior`, `Access`, `Concurrency`, `WriteConcurrency`, `TableOptions` (incl. heir + `decentralized_counters`), `can_read`/`can_write`; unit tests in `src/kind/tests.rs`
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store/mod.rs` → `TermStore` (table-owned arena + copy/free/relinquish/reuse) — the copy boundary
- `src/store/tests.rs` → the `TermStore` unit-test module
- `src/table/mod.rs` → `Table`/`TableId`/`OwnerId`, constructors, metadata/ownership accessors (incl. the live `access` field + `set_access`), write ops, access checks, `select_all`, module wiring
- `src/table/tests.rs` → the table-level unit tests (write ops, metadata, access/protection, ownership/heir, counters, the store-accounting cases)
- `src/table/read.rs` → the read/navigation `impl Table` (incl. `is_named_table`, `tab2list_rev`, and the hash-kind `last`/`prev` aliasing)
- `src/table/rename.rs` → `Table::rename` + `RenameOutcome` (`ets:rename/2` semantics)
- `src/table/shared.rs` → `SharedTable` (the locked handle) + its unit tests
- `src/table/view/mod.rs`, `view/read.rs`, `view/write.rs`, `view/term.rs`, `view/iter.rs` → the guard-scoped `TableRead`/`TableWrite`/`TableTerm` views (the aliasing defence, incl. `TableWrite::set_access`) + the view-borrowed traversal API (iterators / `foldl` / `foldr` / `tab2list_into`)
- `src/table/view/tests.rs` → the view-layer unit tests (incl. the aliasing evidence)
- `src/table/interior.rs` → the private `pub(crate) trait Interior` (incl. the defaulted `iter_objects_rev`) + `build_interior`
- `src/table/access.rs` → `TableAccess` (access-enforcing view, incl. `set_access`)
- `src/table/ownership.rs` → `Heir`/`SetOpt` (incl. `Access(Access)`), the owner-gated `Table::setopts`, `install_heir`/`heir_data`/`copy_heir`/`replace_heir`, the `give_away` semantics + the pid-gap docs
- `src/table/lifecycle.rs` → `OwnerDeath` + `Table::owner_died`
- `src/table/element.rs` → the table-level `update_counter`/`update_element` operations
- `src/table/update.rs` → the counter/element operand machinery (shared with the interiors)
- `src/champ_interior.rs` → `ChampInterior` (CHAMP index) — the **default `set`** interior
- `src/hash_interior.rs` → `HashInterior` (chained index) for `bag`/`duplicate_bag` (and opt-in `set`), `with_keypos` key re-anchoring
- `src/interior_parity.rs` → cross-interior equivalence + default-selection tests (`#[cfg(test)]`)
- `src/interior_bench.rs` → the interior microbenchmarks (`#[cfg(test)]`)
- `src/ordered_interior/{mod,sorted,node,tree,iter}.rs` → the B+-tree `ordered_set` interior (conditional separator refresh via `InsertStep`, single-descent `insert_new`), the sorted-`Vec` alternative, the node arena, the tree ops, the iterator
- `src/ordered_interior/tests.rs` → the ordered-interior unit tests (B+-tree invariants, the sorted-`Vec` interior, the `cmp_arith` key-order cases)
- `src/select/{mod,pattern,expr,exec,run,continuation}.rs` → the match-spec compiler, executor, runner and continuations (`MatchContinuation` carries the table id + registered name + spec + direction + a private `Cursor`: `Begin` / `Anchor(key)` for an `ordered_set` / `Position(ordinal)` for the hash kinds)
- `src/info.rs` → `TableInfo` + `entries` (the 15-field `ets:info` pairs)
- `src/error.rs` → `badarg`/`unsupported` failure constructors
- `tests/common/mod.rs` → shared integration-test helpers; `tests/common/oracle.rs` → the live-`erl` plumbing
- `tests/ets_*.rs` → public-API integration tests by concern (basic/order/select/bag/info/nav/access/set_access/update/replace/continuation/continuation_direction/ownership/fixtable/shared/options/rename/reclamation/concurrency/store_terms), plus `tests/select.rs` and `tests/match_spec_run.rs`
- `tests/differential.rs` + `tests/differential/{battery/*,erl_program.rs}` → the differential evidence suite vs the system `erl` (159 cases, 13 battery groups)
- `ETS_SURFACE.md` → the `ets:*` coverage census + the oracle-probed semantic facts + the runtime-level gaps and the exact API each needs
- `../../erts/emulator/beam/erl_db.c` / `erl_db_hash.c` / `erl_db_tree.c` / `erl_db_util.c` / `erl_db*.h` → the API, interiors, key/match handling and flags being mirrored (read-only)

## See Also
- `ETS_SURFACE.md` → coverage census: every `ets:*` operation mapped to this crate / `beam_bifs` / missing, the oracle-probed facts, and the runtime-level gaps with the exact API each needs.
- `../CONTEXT.md` → crate-layer contracts, the `Table` seam, dependency waves, milestone scope.
- `../../CONTEXT.md` → workspace contracts (§1 A.1/A.8, §2 frozen term layout, §3 error strategy, §7).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`hashing` utilities.
- `../beam_alloc/CONTEXT.md` → the `AllocRef`/`AllocType::Ets` allocation seam `TermStore` uses.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §B/§D → data-structure and ETS copy-trimming discussion.
