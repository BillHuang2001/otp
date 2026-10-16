# `beam_ets` — ETS Tables (Stage 3 data structures)

## Intent
Owns the **observable semantics of `ets:*`** — Erlang Term Storage — over **flat, table-owned term
storage** (the A.1 "copy boundary"): Part I §A.1 (term/key/value semantics), §A.8 (`ets:info/1`
introspection), plus the Stage-3 row of the plan (Part II §H): `set` over a **CHAMP** index,
`bag`/`duplicate_bag` over a **chained-bucket** hash index, `ordered_set` over a **K-ary B+-tree**.

The **interior data structures are NOT externally pinned** (crate-layer `../CONTEXT.md` §2): bucket
layout, hash-map thresholds, tree fan-out and load factors are choices. Only the **semantics** are
contract — which object wins on a duplicate key, key equality/ordering, `size`, and what
`lookup`/`select`/`tab2list`/`first`/`next` return. Term **ordering** and **hashing** are owned by
`beam_term` (`compare::cmp`/`cmp_arith`/`eq`, `hashing::term_hash`) and MUST NOT be re-implemented here.

Legacy ground truth (read-only): `../../erts/emulator/beam/erl_db.c` (API), `erl_db_hash.c`
(`set`/`bag`), `erl_db_tree.c` (`ordered_set`), `erl_db_util.c` (keys/match compiler), headers
`erl_db*.h`. Mirror the SEMANTICS, not the layouts. `ETS_SURFACE.md` is the `ets:*` coverage census
(every operation → this crate / `beam_bifs` / missing, plus the runtime gaps and the API each needs).

### Measured performance — `PERF.md`
This crate's measured memory/perf figures live in **`PERF.md`** (**both sides of every ratio named** —
the table footprint `Table::memory`/`TermStore::words` vs process RSS — plus the run's
`/proc/loadavg`). Headline: **the crate's own ETS adopt/copy/read path is flat (0 MiB/op)** —
1 000 000 same-key upserts keep `TermStore::words`/`blocks` and `Table::memory()` constant, with only a
one-time warm-up step in process RSS. The `copy_bound/ets_copy` bench loop's rust-vs-oracle peak-RSS
growth is therefore **sibling-side**: `beam_rt/src/context/calls.rs`'s nursery growth
(`move_result_into_process_heap` + a one-shot `Process::grow_nursery`, nothing shrinking it back).
**§3** records the `ordered_set` interior itself — the B+-tree vs the sorted `Vec` at several sizes and
key orders, both sides of every ratio plus the run's `/proc/loadavg`.

## Position & dependencies
Crate-layer **wave 1** (`../CONTEXT.md`). Depends only on `beam_common` (errors), `beam_term` (terms +
ordering/hashing), `beam_alloc` (table-owned allocation) and `libc`. Consumed by `beam_bifs` (the
`ets:*` BIFs) and `beam_rt`. No new dependency edges without reporting to `../`. The **atom table is a
per-call parameter**: every op that compares or hashes a term takes `atoms: &AtomTable` (one live global
table owned by `beam_rt`; a `Table` stores no atom state).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_ets::prelude::*`.

### `kind.rs` — kinds, access, options
- `pub enum TableKind { Set, OrderedSet, Bag, DuplicateBag }` (`Default = Set`); `as_atom_name()`,
  `from_atom_name(name)`, `is_multi()`, `is_ordered()`.
- `pub enum Access { Public, Protected, Private }` (`Default = Protected`); `as_atom_name()`,
  `from_atom_name(name)`.
- `pub enum Concurrency { None, Read, Write, ReadWrite }`; `as_atom_name()`, `from_atom_name(name)` —
  **advisory** (parsed/reported, not enforced).
- `pub enum WriteConcurrency { #[default] Off, On, Auto, FixedLocks(u32) }` — the `{write_concurrency,_}`
  mode (the bool is its `Off`/`On` projection): `is_enabled()`, `fixed_locks() -> Option<u32>`,
  `as_atom_name() -> Option<&'static str>` (`"false"`/`"true"`/`"auto"`; `None` for `FixedLocks`),
  `from_atom_name(name)`. `FixedLocks(n)` keeps the **requested** count.
- `pub enum SetInterior { Chained, #[default] Champ }`, `pub enum OrderedInterior { #[default]
  BPlusTree, Sorted }` — crate-internal interior selectors (`as_str()`); `Table::with_set_interior`/
  `with_ordered_interior` install, `set_interior()`/`ordered_interior()` report. Not `TableOptions`
  fields; no BIF surface.
- `pub struct TableOptions` (builder; defaults `Set`, keypos 1, `Protected`, unnamed, uncompressed, no
  concurrency/heir/`decentralized_counters`): `new()`, `set_kind`, `set_keypos(usize) -> Result<&mut Self>`
  (rejects 0 — the only fallible builder), `set_access`, `set_named(bool)`, `set_name(Atom)`,
  `set_compressed(bool)`, `set_write_concurrency(bool)`, `set_read_concurrency(bool)`,
  `set_write_concurrency_mode(WriteConcurrency)`, `set_heir(Option<Heir>)`,
  `set_decentralized_counters(Option<bool>)`; getters `kind()/keypos()/access()/is_named()/
  name() -> Option<Atom>/is_compressed()/write_concurrency()/read_concurrency()/write_concurrency_mode()
  -> WriteConcurrency/heir() -> Option<Heir>/decentralized_counters_option() -> Option<bool>/
  decentralized_counters() -> bool/concurrency() -> Concurrency`; `impl Default`. Contracts: a `private`
  table forces the effective `write_concurrency` off; `set_name` records the name only (not
  `named_table`). Unit tests `src/kind/tests.rs`.
- `pub fn can_read(access, owner, requester) -> bool` / `can_write(…)` — the access matrix
  (`requester == owner` ⇒ everything).

### `key.rs` — object/key handling (the DB key rules)
- `key_of(object, keypos) -> Result<Eterm>` — an object **must be a tuple** with `arity >= keypos`;
  non-tuple / short arity / `keypos == 0` ⇒ `Err(BeamError::BadArg)`. `is_valid_object(object, keypos)
  -> bool`.
- `pub unsafe fn object_elements<'a>(obj) -> &'a [Eterm]` — the tuple payload slice for `select`
  predicates. `# Safety`: `obj` must be a live tuple.

### `store/mod.rs` — table-owned term storage (the copy boundary)
- `pub struct TermStore` — a **never-relocating bump arena** over a `beam_alloc::AllocRef`
  (`AllocType::Ets`) with **per-object reclamation**: `new(alloc)`, `with_default_allocator()`,
  `copy(root) -> Result<Eterm>`, `free(root) -> usize` (reclaims one copied root's exact-word-size runs
  to per-size free lists for reuse; **a no-op returning 0 for a root this store never tracked**),
  `relinquish(root) -> usize` (the `ets:take/2` check-out: valid but never reused, no longer counted
  toward `words()`), `reset()`, `words()`, `blocks()`, `memory()`. `impl Drop` frees every block.
  Deep-copy covers every kind `ets:insert/2` accepts (exact set + residual `BadArg`s in Known Issues).

### `table/mod.rs` — the `Table` handle + ops
- `pub struct TableId(u64)`: `new(u64)`, `raw()`, `next()` (process-global monotonic), `Display`.
  `pub struct OwnerId(u64)`: `new(u64)`, `raw()`.
- `pub struct Table`: metadata + a table-owned `TermStore` + one interior behind the private `Interior`
  trait. Constructors `new(id, opts)`, `with_allocator(id, opts, alloc)`,
  `with_set_interior(id, opts, SetInterior)`, `with_ordered_interior(id, opts, OrderedInterior)`.
  Full signatures live in `src/table/{mod,read,access,element,ownership,lifecycle,rename}.rs`.
- **Shape conventions.** Every fallible op returns `beam_common::Result<_>`; `first`/`last`/`next`/
  `prev` return `Option` (`None` = `'$end_of_table'`). Unless noted an op takes `atoms: &AtomTable`;
  the select/match family and the element updates also take `heap: &mut Heap` (scratch space).
- **Metadata:** `id`, `kind`, `name() -> Option<Atom>` (Some iff a name was recorded), `keypos()`
  (1-based), `access`, `is_named_table() -> bool`, `owner()/set_owner()`, `heir() -> Option<Heir>` /
  `set_heir()` / `heir_data()`, `give_away(caller, new_owner)`, `setopts(caller, SetOpt)`,
  `set_access(&mut self, access: Access)`, `owner_died(dead, is_alive) -> OwnerDeath`, `info() ->
  TableInfo`, `rename(name, is_taken) -> Result<RenameOutcome>`.
- **Writes** (object = the whole tuple; key from `keypos`): `insert` (upsert), `insert_new -> bool`,
  `insert_list(&[object])` (**ATOMIC**), `delete(key)`, `delete_object(object)`, `delete_all_objects()`,
  plus the `pub(crate)` `replace_key(key, &[object])` — the storage-faithful key rebuild behind a
  `duplicate_bag` `select_replace/2`.
- **Reads:** `lookup(key) -> Vec<Eterm>` (0/1 for set/ordered_set), `member`, `lookup_element(key, pos)`
  / `lookup_element_or(key, pos, default)` (scalar; `unsupported` raise for the multi kinds),
  `lookup_element_all(key, pos) -> Vec<Eterm>` / `lookup_element_all_or(..) -> Option<Vec<Eterm>>` (the
  multi kinds' list result), `slot(slot, atoms) -> Result<Option<Vec<Eterm>>>` (`ets:slot/2`: 0-based;
  `None` ⇒ `'$end_of_table'`; negative/out-of-range ⇒ `badarg`), `memory()`, `size()` (objects, not
  keys, for bag/dup_bag), `is_empty()`, `tab2list()` (ordered for ordered_set).
- **Element updates:** `update_counter(key, op)`, `update_counter_with(key, op, default)`,
  `update_element(key, &[(pos, term)]) -> bool`.
- **Navigation:** `first()`, `last()`, `next(key)`, `prev(key)`, `take(key) -> Vec<Eterm>` (copies out +
  reclaims the stored originals), plus `pub(crate) tab2list_rev()`. Hash-kind `last`/`prev` alias
  `first`/`next`; an unknown anchor is `badarg` (`check_navigable`); only `ordered_set` is ordered.
- **Access control:** `check_read(requester)` / `check_write(requester)` / `as_requester(requester) ->
  TableAccess<'_>`.
- **Select family** (`select/exec.rs`; also `select_all(pred)`): `select(spec)`, `select_reverse(spec)`,
  `select_delete(spec) -> usize`, `select_count(spec) -> usize`, `match_object(pattern)`,
  `match_(pattern)`, `match_delete(pattern) -> usize`, `select_replace(spec) -> usize`.
- **Limit + continuation forms** (`continuation.rs`; `Option::None` ⇒ `'$end_of_table'`): each `->
  (Vec<Eterm>, Option<MatchContinuation>)`: `select_limit`/`select_continue`/`select_reverse_limit`/
  `select_reverse_continue`/`match_limit`/`match_continue`/`match_object_limit`/`match_object_continue`.
- `Table::set_fixation(bool)` / `is_fixed()` / `deferred_objects()` — `ets:safe_fixtable/2`.
- `pub fn match_spec_run(list, spec, atoms, heap) -> Result<Vec<Eterm>>` (`run.rs`) — the
  table-independent `ets:match_spec_run/2` runner.
- **`release_empty_interior`** (private) — called after a `delete`/`delete_object` that removed
  something; when the interior is now empty it rebuilds it through `build_interior`, dropping the
  index's grown capacity so `Table::memory()` returns to the empty-table floor (`delete_all_objects`
  already resets). A removal that did not happen never churns the index.
- `pub(crate) trait Interior` — the internal dispatch seam (`insert`/`insert_new`/`delete`/
  `delete_object`/`lookup`/`member`/`lookup_element`/`lookup_elements`/`size`/`iter_objects`/
  `iter_objects_rev`/`keys`/`take`/`slot`/`first_key`/`last_key`/`next_key`/`prev_key`/`memory`/
  `update_counter`/`update_element`). `insert` returns `Result<bool>` — **whether the object was
  retained**, so `Table::insert` reclaims the copy it made when the interior rejects it (`bag`'s
  duplicate is the only `false`). Private — not part of the cross-crate API.

### `table/rename.rs` — `ets:rename/2` (oracle-pinned)
- `pub fn Table::rename(&mut self, name: Atom, is_taken: impl FnOnce(Atom) -> bool) ->
  Result<RenameOutcome>`, `pub enum RenameOutcome { Named { previous_name: Option<Atom> }, Unnamed }`.
  Only the recorded name changes, so objects, `TableId`, owner, access, kind and keypos survive by
  construction. With `named_table`, an already-registered new name (including its own) is `badarg` with
  nothing changed (the caller's `is_taken` predicate reports registry hits) and the BIF returns the
  **new name atom**; **without** `named_table` the rename always succeeds and the BIF returns the
  **unchanged table identifier**. The crate keeps the primitive; the registry rekey, the write gate and
  the non-atom-name/unknown-table `badarg`s are the host's (`ETS_SURFACE.md` §1).

### `table/shared.rs` + `table/view/**` — the shared handle and its guard-scoped views
- `pub struct SharedTable` (`Arc<RwLock<Table>>`): `new`, `id`, `ref_count`, `read() -> Result<TableRead<'_>>`,
  `write() -> Result<TableWrite<'_>>`, `with_read(|v: &mut TableRead<'_>| …)`, `with_write(|v: &mut
  TableWrite<'_>| …)`, `try_into_inner -> Result<Option<Table>>`, `Clone`, `Debug`. A poisoned lock
  yields the **typed** `unsupported_ets: table lock poisoned`. The read/write surface is a
  **lifetime-tagged, guard-scoped view**, so a term read through the shared handle cannot outlive its
  guard (a compile error, pinned by `compile_fail` doctests):
  - `pub struct TableTerm<'g>` — a **private** `Eterm` + `PhantomData<&'g Table>`, with **no** method
    returning a bare `Eterm`. Accessors: `is_integer/is_float/is_atom/is_tuple/is_list/is_bitstring/
    is_binary/is_map/is_pid/is_port/is_reference/is_function`, `as_small_int() -> Option<i64>`,
    `as_atom() -> Option<Atom>`, `as_float() -> Option<f64>`, `tuple_arity()`, `tuple_element(i)`,
    `tuple_elements() -> Vec<TableTerm<'g>>`, `list_to_terms()`, `size()`, `eq_term(Eterm, atoms)
    -> bool`, `cmp_term(Eterm, atoms) -> Ordering`, and the copy-out `copy_into(&mut TermStore)
    -> Result<Eterm>` (the only way to keep a term past the guard).
  - `TableRead<'g>` — owned metadata (`id/size/is_empty/memory/info/kind/keypos/access/owner/name/
    named_table/is_fixed`) plus term reads returning `TableTerm<'g>` (`member`, `lookup`,
    `lookup_element`, `lookup_element_or`, `lookup_element_all`, `lookup_element_all_or`, `first`,
    `last`, `next`, `prev`, `tab2list`, `slot`), and the **copy-out** select family (`select`,
    `select_count`, `match_object`, `match_` take a `&mut TermStore` destination and return owned terms).
  - `TableWrite<'g>` — metadata, mutations over caller-owned inputs (`insert`, `insert_new`,
    `insert_list`, `delete`, `delete_object`, `delete_all_objects`, `update_counter`,
    `update_counter_with`, `update_element`, `select_delete`, `match_delete`, `select_replace`,
    `set_fixation`, `set_owner`, `set_heir`, `set_access`, `setopts`, `give_away`),
    `heir() -> Option<HeirView<'_>>`, `take -> Vec<TableTerm<'_>>`, and `as_requester` (the re-scoped
    raw access-enforcement seam — its reads are bare aliasing `Eterm`s by design, see `view/CONTEXT.md`).
  - `pub struct HeirView<'g>` (`view/term.rs`, re-exported at the crate root): `owner() -> OwnerId`
    (owned) and `data() -> Option<TableTerm<'g>>` (the gift term tagged to the view's borrow).
  - **View-borrowed traversal** (`view/iter.rs`) — `TableRead::{iter_keys, iter_keys_rev, iter_objects,
    iter_objects_rev}` (each yields `TableTerm<'a>` bound to the view borrow via `ets:first|next` /
    `last|prev`; the `*_objects` forms expand the multi kinds per object), `TableRead::{foldl, foldr}`
    over objects, and the bulk copy-out `TableRead::tab2list_into(&mut TermStore) -> Result<Vec<Eterm>>`.
    Iterators are returned as `impl Iterator`, so no private view type leaks into a public signature.
    The views are `!Send` by design (they carry a scratch `Heap`).

### `table/ownership.rs` — `ets:give_away/3` + `{heir, …}` + `{protection, …}` + the heir data
- `pub struct Heir { pub owner: OwnerId, pub data: Option<Eterm> }` — `Some(term)` = `{heir, Pid, Data}`,
  `None` = `{heir, Pid}` (the C `THE_NON_VALUE`). `pub enum SetOpt { Heir(Heir), ClearHeir,
  Access(Access) }` — the parsed `ets:setopts/2` option (the subset modelled).
  `Table::setopts(caller, SetOpt) -> Result<()>` applies the **owner gate first** (`caller != owner` ⇒
  `badarg`, nothing mutated). `give_away` follows the C `ets_give_away_3` order (bad-pid → not-owner →
  self-gift, all `badarg`, owner unchanged on failure).
- `Table::install_heir(owner, data: Option<Eterm>) -> Result<()>` — **copies** the gift data into
  table-owned storage (immediates by value; a non-copyable term is the typed `badarg` and the heir is
  left untouched). `Table::heir_data() -> Option<Eterm>` reads the table-owned copy back.
  `Table::copy_heir(store, heir)` is the shared `pub(super)` copy seam used by `Table::build` and
  `install_heir`. `Table::replace_heir` is the shared `pub(super)` reclaim seam every overwrite site
  (`set_heir`, `setopts`, `install_heir`, `build`) funnels through, so the outgoing heir's copy is freed
  exactly once. The non-copying primitives `set_heir`/`setopts` stay owner-level.
- **`Table::set_access(&mut self, access: Access)` is the post-creation protection setter** (the
  owner-level primitive behind `setopts(_, SetOpt::Access(_))`). `Table` owns the live `access: Access`
  field (initialized from `opts.access()`); `access()`, `info().access`, `check_read` and `check_write`
  all read **that field**. `TableAccess::set_access(&mut self, access: Access) -> Result<()>` is the
  enforcing form (routes through `setopts(self.requester, SetOpt::Access(access))`);
  `TableWrite::set_access` is the guard-scoped mirror. **`TableOptions` is NEVER mutated** — see the
  `{write_concurrency,_}` Design Decision.
- **The pid gap.** No pid term and no process-liveness concept: a `None` new owner stands in for a
  non-pid/dead pid target, an unknown table id is the registry/BIF layer's concern, and a set heir/owner
  renders as `undefined`.

### `table/lifecycle.rs` — the owner-death transition
- `pub enum OwnerDeath { NotOwner, Deleted, Transferred(Heir) }` + `Table::owner_died(dead, is_alive)`.
  `NotOwner` is a complete no-op; `Deleted` (no heir, a self-heir, or a heir the liveness oracle
  rejects) obliges the runtime to drop the table; `Transferred(heir)` sets the owner to `heir.owner`,
  **keeps the heir installed**, and obliges the runtime to deliver `{'ETS-TRANSFER', Tid, dead_pid,
  data}` — **only when `heir.data` is `Some(data)`** — with `Tid` the registered name for a
  `named_table`, else the table id. Neither effect is this crate's.

### `table/access.rs` — the access-enforcing view
- `pub struct TableAccess<'a>` (from `Table::as_requester`) — rechecks `can_read`/`can_write` on every
  op and raises `BeamError::BadArg` on a violation. Write-checked: `insert`/`insert_list`/`insert_new`/
  `delete`/`delete_object`/`delete_all_objects`/`take`/`update_counter`/`update_counter_with`/
  `update_element`/`select_delete`/`match_delete`/`select_replace`/`rename`; read-checked: `lookup`/
  `member`/`lookup_element(+_or/_all/_all_or)`/`select`/`select_reverse`(+_limit/_continue)/
  `select_count`/`match_object`/`match_`/`first`/`last`/`next`/`prev`/`tab2list`/`slot`/`set_fixation`
  (the oracle gates `safe_fixtable/2` as a READ) plus the `/3`+`/1` limited forms. `ets:match_spec_run/2`
  has no access concept. `ets:info/1,2` is deliberately **not** gated. `give_away(new_owner)` has no
  protection check (ownership is not a read/write op); `setopts`/`set_access` are owner-gated.

### `table/update.rs` — `update_counter`/`update_element` operand machinery
- `pub(crate)` helpers shared with the hash interior: `counter_slot`, `add_counters`,
  `replace_elements` (re-exported from `table/mod.rs`), plus the decode/build internals
  (`decode_counter_ops`/`decode_counter_tuple`/`counter_operand`/`CounterOp`/…).

### `champ_interior.rs` — CHAMP open-addressing interior (**default `set`**)
- `pub struct ChampInterior` + `Default`/`new()`: each level consumes `WIDTH = 5` bits of the 64-bit
  `term_hash`, compressing children into a `datamap` (inline slots) + `nodemap` (sub-nodes); a full-hash
  collision lives in a `CollisionNode` (`eq`-compared), so a pathological hash cannot recurse past 64
  bits. Every `Interior` method is overridden; `first_key`/`next_key` use the trait defaults while
  `last_key`/`prev_key`/`iter_objects_rev` apply the hash-kind aliasing. **Delete canonicalization**: a
  removal canonicalizes bottom-up — a sub-node holding exactly one entry is **inlined** into its parent
  as an inline slot (a sub-node is indexed by the *next* 5 hash bits, so promoting a multi-entry
  sub-node wholesale would break routing).

### `hash_interior.rs` — chained hash-index interior (`bag`/`duplicate_bag`; opt-in `set`)
- `pub struct HashInterior`: buckets `Vec<Vec<Entry>>` (`Entry { key, objs: Vec<Eterm> }`), lookup by
  `term_hash(key) % buckets` then `eq`; doubles + rehashes past a load factor. `set` keeps ≤ 1 object
  per key; `bag` rejects a duplicate *object* within a key (its `insert` returns `false`, so the caller
  reclaims the rejected copy); `duplicate_bag` keeps a multiset. `delete_object` removes **every** equal
  object (the oracle). Public API for testing/reuse: `new(kind)`, `take(key, atoms)`,
  `update_counter(key, pos, incr, atoms)`, `update_element(key, updates, atoms)`, `memory(atoms)`,
  `bucket_count()` (the `nactive` boundary), plus `keys()` and the `Interior::slot` bucket override.

### `ordered_interior/` — the ordered `ordered_set` interiors (B+-tree + sorted `Vec`)
- `mod.rs` — `OrderedSetInterior` (K-ary B+-tree): `new()`, `first`/`last`, `first_key`/`last_key`,
  `next`/`prev`, `next_from(bound, inclusive)`/`prev_from(bound, inclusive)`, all via
  `beam_term::compare::cmp_arith` (`erl_db_tree.c` `cmp_key_eq`, `CMP = erts_cmp(a, b, 0, 0)`), so `1`
  and `1.0` are the same key; whole-object identity (`delete_object/2`) stays the exact `compare::eq`.
  Plus its `Interior` impl; the node arena **is** counted by `memory()`.
- `sorted.rs` — `SortedOrderedInterior`: the same semantics over one sorted `Vec` (O(log n) lookup via
  the shared `node::search_entries`, O(n) insert/remove), same navigation, capacity-based `memory()`;
  selectable via `OrderedInterior::Sorted`.
- `node.rs` — the arena node layout, `ORDER`/`MIN_OCCUPANCY`, the `NodeStore` (`NodeId` slots + free
  list; nodes never relocate), the shared `Entry` record, `search_entries`, `words_of`.
- `tree.rs` — insert/lookup/delete with split + borrow-or-merge rebalance, the replace-key-term rule,
  the separator invariant `keys[i] == max(child[i])`. `iter.rs` — the leaf-chain ordered iterator.

### `select/` — the match-spec compiler + executor
- `mod.rs` — `pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;`
  `pub struct MatchSpec { pub clauses: Vec<MatchClause> }`, `pub struct MatchClause { head, guard, body }`,
  `pub fn compile(spec, atoms) -> Result<MatchSpec>`; aliases `MatchPattern`/`MatchElement` = `Pattern`,
  `MatchGuard`/`MatchBody`/`MatchBodyElement` = `Expr`.
- `pattern.rs` — `Pattern` (nested tuple/list/improper patterns, quoted literals, `'_'`, `'$N'` binds
  with repeat-occurrence equality; every other atom — including `'$_'`/`'$$'`, special only in a *body*
  — is an exact `=:=` literal).
- `expr.rs` — `Expr`/`CmpOp`/`TypeTest`/`ArithOp`: the guard/body language. Guards: `andalso`/`and`,
  `orelse`/`or`, `not`, `const`, bare truthiness; comparisons `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`;
  the type tests `is_atom/integer/float/number/tuple/list/binary/bitstring/map/pid/port/reference/
  function/boolean`; the guard BIFs `element/2`, `abs/1`, `size/1`, `hd/1`, `tl/1`, `length/1`,
  `map_get/2`, `is_map_key/2`, `map_size/1`, `tuple_size/1`, `byte_size/1`, `bit_size/1`, `float/1`,
  `trunc/1`, `round/1`, `ceil/1`, `floor/1`, `bnot/1`, and `self/0` (through the thread-local `SelfHook`
  seam; default is a typed `badarg`). Map **construction** is supported in guards and bodies, and
  map/tuple/binary operands are read through `beam_term` primitives. **Arithmetic** (`+ - * / div rem
  band bor bxor bsl bsr`, unary `+`/`-`) routes through the `pub trait ArithHook` seam (default methods)
  with `DefaultArithHook` installed via a `thread_local` (`set_arith_hook`) — no crate edge to
  `beam_bifs`. Bodies: sequences (value = last expression), tuple/list/**map** construction, literals,
  `'$N'`, `'$_'`, `'$$'`. Guard truth is an internal `Expr::Bool` (independent of the atom table).
  Anything outside the recognised set ⇒ `compile`/`expr` `badarg`.
- `exec.rs` — `impl Table`'s select family + `pub enum BodyKind` (`WholeObject`/`AllBindings`/
  `LiteralTrue`) that reuses the compiler for `ets:match*`. A BODY runtime failure is replaced by the
  atom `EXIT` (`select::exec::exit_term`); `guards_hold` returns `Result<bool>` — a guard the oracle
  would itself raise or a non-boolean guard skips the clause, while a capability gap propagates as a
  typed error.
- `run.rs` — `match_spec_run(list, spec, atoms, heap)`.
- `continuation.rs` — `pub struct MatchContinuation` (opaque: table id + the table's **registered-name
  atom** for a `named_table` (else `None`) + the compiled spec (body included) + limit + direction + a
  private `Cursor`) and the limited forms. `Cursor` is `Begin` (a `/3` start) / `Anchor(key)` (an
  `ordered_set` resume) / `Position(ordinal)` (a hash kind's resume ordinal). `resume` rejects a
  continuation whose table id **or** recorded name does not match, and re-applies the stored direction
  and spec so a `/1` resume needs no direction/spec argument.

### `info.rs` — introspection (`ets:info`)
- `pub struct TableInfo { id, name: Option<Atom>, kind, keypos, access, size, memory, owner:
  Option<OwnerId>, heir: Option<OwnerId>, named_table: bool, compressed: bool, write_concurrency: bool,
  read_concurrency: bool, decentralized_counters: bool }`;
  `entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)>` — the heap-free `{Key, Value}` pairs of
  the oracle's **15** `ets:info/1` fields in the oracle's order (`id`, `decentralized_counters`,
  `read_concurrency`, `write_concurrency`, `compressed`, `memory`, `owner`, `heir`, `name`, `size`,
  `node`, `named_table`, `type`, `keypos`, `protection`). The sibling `beam_bifs` bridge builds the
  observable proplist (same 15-field order).

### `error.rs` — failure constructors
- `pub(crate) fn badarg() -> BeamError`; `pub(crate) fn unsupported(what: &str) -> BeamError` (a
  `BeamError::Raise`, error class, reason atom `unsupported_ets: <what>`). No crate-local error enum.

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
  `src/hash_interior.rs` (~1074), one cohesive chained-index interior kept whole rather than split.
  Large `tests/` modules are legitimate (the embedded oracle program `tests/differential/erl_program.rs`
  and `tests/select.rs`); split-out unit-test siblings: `src/{table,ordered_interior,store,kind}/tests.rs`.
- **Licensing** Apache-2.0; no new external deps (only `beam_common`/`beam_term`/`beam_alloc`/`libc`).

## Design Decisions
- **The shared handle is guard-scoped, not copy-on-read.** `TableRead`/`TableWrite` hold the `RwLock`
  guard and every term they yield is a `TableTerm<'g>` bound to the view's borrow, so a term read under
  one guard cannot be used after it is dropped — a **compile error** (two `compile_fail` doctests), not
  a documentation caveat. Cost on the default path is **zero**; allocation happens only via
  `TableTerm::copy_into`/the copy-out `select` forms. The raw `Table`/`TableAccess` read family still
  returns bare `Eterm`s — a cross-crate API decision (hosts consume it); `ETS_SURFACE.md` §7.6 lists the
  exact names.
- **Heir gift data is table-owned; arity 2 is representable.** `Heir.data: Option<Eterm>` distinguishes
  `{heir, Pid}` from `{heir, Pid, undefined}`. The data is deep-copied into the table store at creation
  and at `install_heir`, and reclaimed by `replace_heir` at every overwrite site — the C
  `ERTS_ALC_T_DB_HEIR_DATA`/`free_heir_data` pair; storing the caller's term verbatim would dangle once
  the installing process's heap died.
- **`ordered_set` defaults to a K-ary B+-tree** (`ORDER = 32`, `MIN_OCCUPANCY = 16`) with an
  arena-backed, non-relocating `NodeStore`, a leaf chain and `O(log n)` point lookup; keys compare by
  `cmp_arith`. Insert is `O(log n)` node visits amortized (worst case `O(log² n)` on a cascading root
  split); delete is `O(log² n)` node visits but only `O(log n)` term comparisons — see
  `src/ordered_interior/CONTEXT.md`. The sorted-`Vec` interior stays selectable (`OrderedInterior::Sorted`)
  as a comparison index; the interior shape is unpinned and the measured numbers are in `PERF.md`.
- **`set` defaults to CHAMP; `bag`/`duplicate_bag` stay chained.** Measurement-driven (index only): CHAMP
  wins on insert, iteration and index bytes/entry (2–7× fewer), is comparable on lookup-hit, and loses on
  delete (canonicalization) — the insert/iterate/memory profile ETS is optimised for. `SetInterior::
  default() == Champ`; `Chained` stays selectable for `set` and is still the index for the multi kinds.
  `src/interior_parity.rs` proves the observable traces identical.
- **Interiors are a private `Interior` trait, not a pinned layout.** Generic defaults (rebuild-on-heap)
  with hash overrides (in-place destructive element writes) so the B+-tree needs no writable index slot.
- **One `TermStore` per table; copy-on-insert, per-object free.** `free(root)` returns a removed root's
  exact-word-size runs to per-size free lists that `copy` reuses before bumping; it is a no-op for a root
  the store never tracked (which is what makes the heir reclaim path safe).
- **`take` returns checked-out copies and reclaims the stored originals.** Copy-before-free is mandatory
  (freeing first would let a later same-size `copy` reuse the words the caller holds), and each returned
  copy is `relinquish`ed: valid for the table's lifetime, never handed out again, no longer counted
  toward `memory()`.
- **An emptied interior releases its index storage.** Once the last object has left, `delete`/
  `delete_object` rebuild the interior through the private `release_empty_interior` — the same operation
  `delete_all_objects` uses — so `ets:info(T, memory)` returns to the empty-table floor the oracle
  reports. Residual (capacity vs live data): an index shrunk to **one** object still reports the bucket
  array/nodes it grew; per-bucket capacity release (hash kinds) and an ordered-arena shrink on delete are
  the follow-up — a throughput/accounting refinement, not a leak. See `PERF.md` §2.
- **An interior's stored key term aliases a live, table-owned object.** `key_of(object, keypos)` returns
  a pointer *into* the object's table-owned `TermStore` storage, so an interior that keeps a key must
  re-point it whenever it replaces or removes the object it aliased: on a `set` upsert
  (`hash_interior`/`champ_interior`) and, for `bag`/`duplicate_bag`, when `delete_object` removes the
  object whose key element the entry's stored key pointed at (re-derived from a surviving object at
  `keypos`). `Table::relocate_rebuilt_object` derives the index key from the **table-owned copy**, never
  the caller's key. See `tests/ets_reclamation.rs`.
- **Access control mirrors `db_check_access`.** `can_read`/`can_write` treat `requester == owner` as
  full access; `TableAccess` is the enforcing view while the raw `Table` ops stay owner-level primitives.
- **`ets:info/1` mirrors the oracle's 15 fields in the oracle's order** (the reverse of the C `fields[]`
  array in `erl_db.c:4406`); `heir`/`node` are runtime-shape constants, `decentralized_counters` is the
  resolved value: effective `write_concurrency` **and** the explicit `{decentralized_counters, Bool}`
  when given, else `kind.is_ordered()` (the CATREE default). `private` normalizes the concurrency flags
  off, which makes `decentralized_counters` false too.
- **Ownership/heir semantics are a `Table`-level concern, not an access-mode one.** `give_away/3` and
  `setopts/2` do not consult `protection`; they are owner-gated. `ets:info` renders only the heir's pid.
- **`insert_list` is atomic; `select_replace/2` is a two-phase rewrite with the C's static body check**
  (a body outside `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}` aborts with `badarg` — even with
  no match — leaving the table unchanged; a valid body is applied atomically).
- **A `duplicate_bag` replacement rebuilds the affected key and reclaims the objects it replaces.**
  Several objects share a key and the replacement must land on the **specific** matched object, so the
  key's whole object list is rebuilt: the current objects are read through `Table::lookup`, each
  replacement is computed on the caller's scratch heap, and `Table::replace_key` deep-copies the new
  list into table storage **first** and only then removes the old objects through the ordinary freeing
  path — copying before freeing guarantees no replacement reads reclaimed storage and keeps the store's
  free-run reuse at a steady state.
- **Match-spec arithmetic routes through the `ArithHook` seam** (a `thread_local`), keeping wave-1
  `beam_ets` free of a `beam_bifs` dependency; candidate to lift into `beam_term` later.
- **Continuations are opaque owned values, not terms** (table id + compiled spec + limit + position),
  so `select_continue` resumes without re-compiling; `Option::None` is the crate's `'$end_of_table'`.
- **`ets:slot/2` is one contract over two decompositions.** The defaulted `Interior::slot` is a
  **one-object-per-slot** partition, exactly right for the two `ordered_set` interiors **and** the CHAMP
  `set`; the chained `HashInterior` overrides it with its bucket array (`nactive` = the bucket count).
  An `ordered_set` slot is **exact** — the object with the (n+1)-th smallest key (`erl_db_tree.c:1139`);
  a hash slot is a hash *bucket* whose count and contents are the interior's own (`../CONTEXT.md` §2),
  so only the partition / single end-of-table slot / `badarg` boundary is contract.
- **A BODY runtime failure yields the atom `EXIT`; a GUARD failure skips the clause.** A guard that would
  raise or is non-`true`/non-boolean skips the clause; a capability gap (an unevaluable guard) propagates
  as a typed error. Guard truth is an internal boolean (`Expr::Bool`); boolean *bodies* still render as
  the interned atoms `true`/`false`.
- **Small contract points.** `match_delete` returns the deleted count, not the BIF's `true`. `size`
  counts objects, not keys. `&AtomTable` is passed per call — a `Table` cannot own atom state (indices
  are global). The registered `name` and the `named_table` flag are independent (`set_name` records the
  name only).
- **Hash-kind navigation order is unspecified (a deliberate freedom).** Every key a walk yields is
  present, `first`+`next` is a permutation of the stored keys, and `next`/`prev` on an absent anchor are
  `badarg`; `ordered_set` navigation is exact (ascending arithmetic key order; absent keys read as
  bounds).
- **B+-tree branch separators are re-materialised from the rightmost live leaf, but only where the
  subtree max actually moves.** `Tree::insert_rec` returns an `InsertStep` carrying the ascending path's
  new maximum, so a separator is rewritten only when the subtree maximum it names changed (or a split
  creates a new one). A replaced separator must be re-materialised (`NodeStore::subtree_max_key`): an
  upsert reclaims the object that established the subtree maximum, so a stored separator term would
  dangle even when the key *value* is unchanged. The descent **binary-searches** the strictly ascending
  separators (`O(log ORDER)`, ≤5 `cmp_arith`); `Tree::check_invariants` asserts separator **term identity**.
- **`insert_new` is a single descent.** The same `insert_rec` runs with a refuse-on-existing-key flag,
  stopping with `InsertStep::Refused` and mutating nothing, so `ets:insert_new/2` performs no separate
  `find` before its `insert`.
- **`{write_concurrency,_}` is a mode, not a bool.** `WriteConcurrency` (`Off`/`On`/`Auto`/`FixedLocks`)
  is the model; the bool API is its `Off`/`On` projection, so `beam_bifs` stays source-compatible, and
  `decentralized_counters` is derived from the mode. The fine-locking normalization is a **creation-time**
  property of `TableOptions`: a runtime `set_access` change does **not** re-normalize it (the oracle
  agrees).
- **The hash kinds' `last`/`prev` ARE their `first`/`next` (C-faithful navigation).** ERTS's hash
  interior installs the same function twice (`erl_db_hash.c:853-856`) and `db_select_reverse_hash`
  delegates to the same chunk function as `db_select_hash` (`:2310-2313`). So for `set`/`bag`/
  `duplicate_bag` there is **no** backward walk: `ets:last/1` == `ets:first/1`, `ets:prev/2` ==
  `ets:next/2` (an unknown anchor stays `badarg` through `Table::check_navigable`), and
  `ets:select_reverse` returns exactly what `ets:select` returns. `ChampInterior`/`HashInterior`
  reproduce it by overriding `last_key`/`prev_key`/`iter_objects_rev`; the `Interior` trait's defaults
  stay the `ordered_set` semantics.
- **Select-family continuations are key-anchored on an `ordered_set`, ordinal on the hash kinds.**
  `MatchContinuation`'s private `Cursor::{Begin, Anchor(Eterm), Position(usize)}` is chosen by the table
  kind (which never changes after creation), so the two shapes are never mixed. The ordered kind resumes
  from the **last key its chunk yielded** (`Anchor`), an **exclusive** range bound, so the resume
  survives the anchor's own deletion. The hash kinds keep the ordinal `Position`, because their ERTS
  cursor is a bucket/index, not a key, and this crate documents the hash iteration order as an interior
  freedom. Continuation identity is the table id + the `named_table` name atom; the read gate precedes
  consumption.

## Concurrency, fixation & sharing (current state)
- **`Table` is `Send + Sync` and shareable across processes**: plain `Eterm` data + a
  `Box<dyn Interior>` (`Interior: Send + Sync`) + a `TermStore` (`unsafe impl Send/Sync` with a
  rationale: no interior mutability, every storage-reaching method takes `&mut self`).
- **`SharedTable`'s coarse lock is the whole story — no lost updates.** One `write()` guard spans a
  whole check-then-act sequence (`insert_new`, `update_counter`, `select_replace`), so concurrent
  writers lose nothing; fine-grained `{write_concurrency}` affects throughput only, never results. A
  poisoned lock is a typed error. Reads through the handle are guard-scoped (see the guard-scoped-view
  Design Decision).
- **`ets:safe_fixtable/2` fixation = deferred reclamation.** While fixed, every removed object's storage
  goes onto `deferred_frees` (and `delete_all_objects` defers each root instead of `reset()`ing);
  unfixing flushes them, so `ets:info(T, memory)` is held while fixed and falls at unfix.
  `deferred_frees` is not counted in `memory()`.
- **Subterm sharing (the copy-boundary trim).** `TermStore::copy_term` shares exactly the one literal
  whose liveness the crate can prove — the global empty tuple (`copy.c:130-140`);
  `beam_term::literal::LiteralArea` literals are deep-copied (the area may be dropped and the crate
  holds no handle). `{keypos, N}` keys live inside the stored tuple; `compressed` is parsed/reported
  with no storage effect.

## Test Strategy
- Per-module `#[cfg(test)]` tests in every `src/` module (`store/tests.rs`, `kind/tests.rs`,
  `table/tests.rs`, `table/view/tests.rs`, `ordered_interior/tests.rs` hold the big ones;
  `interior_parity.rs`/`interior_bench.rs` are `#[cfg(test)]` siblings). Coverage: kind/options, key
  validation, store copy/free/reuse/relinquish, table insert/take accounting, hash-interior ops, CHAMP
  ops (round-trip, 2000-key churn, collision node, canonicalization, memory monotonicity), B+-tree
  invariants (incl. 10 000-key churn), the sorted-`Vec` interior, select compile/execute, info,
  owner-death, the heir forms/reclaim, the duplicate_bag `select_replace` rebuild storage floor, and the
  guard-scoped views (incl. the aliasing defence).
- **Cross-interior equivalence + defaults** (`interior_parity.rs`): a shared LCG script drives chained vs
  CHAMP (identical traces) and B+-tree vs sorted `Vec` (order-sensitive tail), pins the default
  selectors, and pins the `cmp`→`cmp_arith` arithmetic-key behaviour.
- **Microbenchmarks** (`interior_bench.rs`): chained vs CHAMP and B+-tree vs sorted `Vec`, with in-line
  correctness assertions. Run with `… -p beam_ets --lib interior_microbenchmark -- --nocapture` (or
  `ordered_interior_microbenchmark`); sizes/shapes via `BEAM_ETS_BENCH_SIZES`/`BEAM_ETS_BENCH_SHAPES`,
  plus the ordered interior's **key-order** dimension via
  `BEAM_ETS_BENCH_ORDER=shuffled|ascending|descending|all` (default `shuffled`) — the order is
  irrelevant to the B+-tree and decisive for the sorted `Vec`, so both interiors' best *and* worst cases
  get measured. The measured outcomes live in `PERF.md` (§3 for the ordered interior).
- Crate-level integration over the **public API**: `tests/ets_*.rs`, `tests/select.rs`,
  `tests/match_spec_run.rs`, helpers from `tests/common/` and the live-`erl` plumbing in
  `tests/common/oracle.rs`. Reclamation/accounting files:
  - `ets_memory.rs` → the in-crate memory characterisation of the `copy_bound/ets_copy` loop shape
    (`TermStore::copy`/`free` plus the full `Table::insert`/`lookup` loop): the store's `words`/`blocks`
    and `Table::memory()` stay flat across N upserts (one test — `VmHWM` is process-wide).
  - `ets_copy_oracle.rs` → the **oracle's** side of the same loop (`escript`): a one-key `set` table's
    `ets:info(T, memory)` over a 20× range of N and the oracle child's `VmHWM`.
  - `ets_reclamation_oracle.rs` → the oracle-cross-checked reclamation accounting: the delete family, the
    upsert progression and the heir gift data, each `ets:info(T, memory)` fact cross-checked against the
    live `erl`.
  - `ets_reclamation.rs` pins per-object reclamation across every kind and interior shape (the store's
    `words`/`blocks` floors across churn), including the emptied-interior release; `ets_store_terms.rs`
    cross-checks every newly-copyable boxed kind through `Table::insert`/`lookup` against the oracle's
    `phash2`; `ets_replace.rs` pins that 100 repeats of the identical `duplicate_bag` replacement stay
    oracle-identical with a flat `Table::memory`; `ets_slot.rs` pins `Table::slot`'s ordered-exact /
    hash-contract split, its read gate and the guard-scoped `TableRead::slot`; `ets_info.rs` the
    `size`/`memory` counters (incl. the `ordered_set` node-arena share and `safe_fixtable`'s held memory).
- **Access enforcement, per-operation, against the live oracle.** `tests/ets_access.rs`
  (`every_read_and_write_op_of_the_access_matrix_matches_the_live_oracle`) drives **every** read, every
  write, every `ets:info` field, every ownership op and the four continuation resumes, under owner +
  non-owner × `public`/`protected`/`private`, asserting each cell's crate outcome equals the live
  oracle's **and** the oracle's `{SizeBefore,SizeAfter,is_list(ets:info(T))}` intact triple (so a denial
  is `badarg` *and* left the table untouched).
- **Access-enforcement audit — verdict: no divergence** (`tests/ets_access_audit.rs`). An audit of the
  owner-vs-other × `public`/`protected`/`private` gate for **every** table op (each verb's C mask in
  `erl_db.c`/`erl_db_util.h` — `DB_INFO` never fails, `DB_READ = DB_PROTECTED|DB_PUBLIC`, `DB_WRITE =
  DB_PUBLIC`) against the live oracle. It closes the corners the matrix does not reach:
  `ets:lookup_element/4` present+absent, the `false` direction of `safe_fixtable/2`, the bag
  `lookup_element` gating-before-scalar-refusal, all 15 `ets:info/2` fields ungated, the stdlib wrapper
  gates `ets:foldl/foldr`/`tab2list`/`match_delete`, and `setopts(T,[])`/`{protection,_}` owner-gating.
  The sibling `beam_bifs` gate assignments were verified correct against the C.
- **Differential evidence suite** (the compatibility gate): `tests/differential.rs` runs **191 cases**
  (15 case-group files under `tests/differential/battery/` plus the shared harness `battery/mod.rs` and
  the embedded oracle program `erl_program.rs`) identically under the system `erl` (OTP 29.1.1 /
  erts-17.1) and under `beam_ets`, comparing canonical renderings line-by-line — **every case agrees**;
  `KNOWN_DIVERGENCES` is empty. The `multikind` group pins the hash-kind navigation/reverse aliasing,
  `slot` pins `ets:slot/2`, `maps` the head-map/map-guard/map-body subset. The oracle is **required by
  default** — a genuinely absent `erl` FAILS the cross-check rather than skipping it
  (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out with a loud skip; `BEAM_ETS_REQUIRE_ORACLE=1` forces it).
- **Randomised `ordered_set` differential** (`tests/ets_ordered_oracle.rs`, `#[ignore]`d by default):
  a 73-term mixed-type key pool driven by a SplitMix64-finalised LCG through a **60-slot** op space
  (34 `insert` slots, one slot each for `insert_new`, `delete`, `delete_object`, `take`,
  `delete_all_objects`, `lookup`, `member`, `first`, `last`, `next`, `prev`, `select`, `select_count`,
  `match_object`, `select_reverse`, `foldl`, `update_counter`, `update_element`, `foldr`,
  `lookup_element/3`, `lookup_element/4`, plus a **`next`/`prev` walk** dimension — a full chain and a
  chain from the `aux` key (usually absent, i.e. a range bound) — and a **key-boundary** slot
  `insert_block` inserting a fresh contiguous ascending integer block longer than `ORDER = 32`, so every
  firing forces a split), rendering every observed key as its pool index and comparing
  `{size, in-order tab2list, op result}` after every step against the live oracle — 1800 steps / 3 seeds
  agree, **zero divergence** (peak **68** live keys, longest single walk **67** keys). Run it with
  `--ignored`; the test asserts its own peak and that every new slot fired, so a future op-mix change
  cannot silently stop exercising node splits, the merge-on-delete churn, or a walk across a split
  boundary. A deterministic, always-on counterpart (a `first`+`next`/`last`+`prev` chain over 100 keys
  interleaved with mutation, plus an absent-key anchor across the split) lives in `tests/ets_fixtable.rs`.
- **`tests/ets_continuation.rs` / `ets_continuation_direction.rs`** cross-check the continuation cursor
  against the live oracle **including a mutation between chunks**: an `ordered_set` resume survives a
  delete of the last-yielded (anchor) key, and a delete/insert behind the cursor does not shift the next
  chunk.
- **Gate (this crate):** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml
  -p beam_ets --no-fail-fast` is green — **615 passed / 0 failed / 1 ignored across all binaries
  (incl. 10 doctests, 6 of them `compile_fail`)**. The ignored test is the randomised `ordered_set`
  differential, which passes under `--ignored`. With the oracle required, the battery emits and compares
  all **191** cases. Forced-oracle: `BEAM_ETS_REQUIRE_ORACLE=1 … --test differential`.

## Known Issues (honest gaps — not full parity)
- **The raw `Table`/`TableAccess` read family still returns bare `Eterm`s that alias table storage** —
  the hazard class the shared handle closes at compile time: a caller can hold such a term past its
  borrow (or past `take`, or past the table itself) and then observe words reclaimed by a later same-size
  insert. The one exception inside the tagged views is `TableWrite::as_requester`, whose READ results are
  bare aliasing `Eterm`s **by design**
  (`view/tests.rs::as_requester_reads_are_a_documented_residual_hazard`). The raw API fix is a cross-crate
  decision — `ETS_SURFACE.md` §7.6 names every method that should move behind the tagged-term discipline.
- **`Concurrency` is advisory.** `read_concurrency`/`write_concurrency` are parsed and reported but there
  is no fine-grained locking; the only synchronisation is `SharedTable`'s coarse `Arc<RwLock>`. The
  `{write_concurrency, auto}` and `{debug_hash_fixed_number_of_locks, N}` modes **are** representable
  (`WriteConcurrency::Auto`/`FixedLocks(n)`); what remains host-side is the *rendering* (the `auto` atom
  / the normalized lock tuple comes from the `beam_bifs` `TableProfile` sidecar). Fine-grained locking
  is a **throughput** workstream, never a correctness gap.
- **Indices report allocated capacity, not live data.** An index shrunk to one object still reports the
  bucket array/nodes it grew (see the emptied-interior Design Decision and `PERF.md` §2).
- **The owner-death transition's runtime caller reclaims named and unnamed tables alike.** `Table::owner_died`
  is driven by `beam_rt::runtime::ets_owner::drain_ets_owner_exits` (over the `EtsExitObserver` on the
  `ProcessTable`), and `beam_bifs`'s `ets:new/2` records the owner. The set of tables a dead owner owned
  is the `beam_bifs::TableRegistry::handles_owned_by` roster — **unnamed** tables included — with the
  `OwnerIndex` recorded at `ets:new/2` unioned in for the **registered name** a `named_table`'s drop must
  release (`ETS_SURFACE.md` §7.4). **Ownership modelling has no pid/liveness concept**: `give_away/3`
  treats `None` as the oracle's non-pid/dead-pid target (both `badarg`); delivering the transfer message
  and dropping the table stay the runtime's job.
- **`TableInfo::entries` renders `owner` as `undefined` and `id` as a small integer.** The crate's own
  15-field proplist has no pid/ref term; the observable `ets:info/1,2` bridge in `beam_bifs` substitutes
  the recorded owner pid and the tid reference, so the BIF output matches the oracle.
- **`ets:setopts/2` is modelled for `{heir, …}` and `{protection, Access}`.** `SetOpt` carries
  `Heir(Heir)`/`ClearHeir`/`Access(Access)`; other options (`safe_fixtable`, `{write_concurrency, _}`,
  …) are out of scope; `ets:info(T, holder)` is `badarg` in the oracle and none is emitted. The BIF
  bridge's `{protection, _}` wiring belongs to `beam_bifs`.
- **Scalar `lookup_element` is a deliberate typed refusal for the multi kinds.** `Table::lookup_element`/
  `lookup_element_or` raise the typed `unsupported_ets` for `bag`/`duplicate_bag` — a scalar `Eterm`
  cannot express the oracle's *list* — and the faithful forms are
  `lookup_element_all`/`lookup_element_all_or`. The `beam_bifs` BIFs branch on `kind().is_multi()`
  (`beam_bifs/src/ets.rs::lookup_element_3/4`), so the refusal is unreachable through the BIF surface.
- **`tab2file`/`file2tab`/`tabfile_info` are BLOCKED, not merely out of scope.** The table-file format is
  `{Header, Objects, Footer}` with every field an ETF term, so it needs `beam_codec` plus file I/O
  (`beam_io` is wave 5 — an upward edge, forbidden for a wave-1 crate). `tab2list` + `first`/`next`
  remain the enumeration path (`ETS_SURFACE.md` §5).
- **Operations outside this crate's scope** (all present in OTP 29): `update_element/4`, the
  `foldl`/`foldr` **BIFs** (the view layer has `TableRead::{foldl,foldr}` traversal, but the BIFs need a
  `FunCaller`), `whereis/1`, `table/1,2`, `tab2file*`/`file2tab*` — `ETS_SURFACE.md` §1–§5 names the
  owner and the blocker for every one. (`ets:clear/1` and `ets:size/1` do not exist in OTP 29, so their
  absence is correct.) The `/3` limited forms are implemented; their opaque continuation terms and the
  bare-vs-`{Results,'$end_of_table'}` shaping are `beam_bifs`'s. `select_reverse/1,2,3` is a crate
  primitive **and** is adopted by `beam_bifs::ets::reverse` (read-gated).
- **`match_spec_run` accepts a RAW spec, unlike the oracle.** The oracle requires
  `ets:match_spec_compile/1` first (a raw clause list is `badarg`); this crate has no compiled-spec
  *term*, so `run.rs` compiles the raw spec and the bridge maps `match_spec_compile/1` onto the compiled
  `MatchSpec`.
- **Named-table lookup-by-name is the runtime's job.** `Table` exposes `name()`/`is_named()` only; the
  name→table registry is the sibling `beam_bifs::ets::TableRegistry` (this crate holds no global state).
- **A hash-kind continuation is an ordinal into iteration order; an `ordered_set`'s is a key anchor.**
  The oracle's `ets:select/1` continuation carries the last key and resumes relative to it; this crate
  reproduces that for `ordered_set` (`Cursor::Anchor`, an **exclusive** `Table::next`/`prev` bound) —
  oracle-verified across a mutation *including the anchor's own deletion*. The hash kinds keep the
  ordinal `Position` (ERTS's cursor there is a bucket/index), so a mutation at or behind the ordinal can
  skip or repeat an object relative to the oracle across a resume straddling a mutation — never a typed
  error and never a wrong result for a mutation-free walk.
- **Deep-copy covers every kind the oracle accepts in `ets:insert/2`** — immediates, cons cells, tuples,
  flat **and hash** maps, funs (all three kinds), boxed leaves (float, bignum, internal ref, on-heap
  bitstring), external pid/port/reference, and sub-bitstrings / off-heap refc binaries (re-materialised
  as self-contained on-heap bitstrings). The residual typed gaps (`BadArg`, never a wrong result) are a
  bare `BinRef` root, a native record (`RECORD_SUBTAG`), the reserved boxed subtags, a bare header word,
  and a malformed `ErlSubBits`. Evidence: `src/store/tests.rs`, `tests/ets_store_terms.rs`.
- **`ets:take/2` retains the checked-out object's arena until the table is dropped.** `Table::take`
  deep-copies the removed object out and `relinquish`es the copy: `store.words()` drops by exactly the
  object's word count (so `Table::memory`/`size` stay correct and flat across a take + re-insert churn),
  but the words stay **valid** and never return to the free lists, so a repeated take churn grows the
  store's `blocks` (bounded by the total taken volume) until `store.reset()`
  (`ets:delete_all_objects/1`) or the table is dropped — a checked-out term must stay valid and `take`
  hands back table-owned `Eterm`s.
- **The match-spec compiler's typed divergences.** Head-**map** patterns, map **guards** and map
  **bodies** are oracle parity (the `maps` battery group: literal top-level keys, structural values,
  extra keys tolerated, `#{}` matches any map, `1` ≠ `1.0` keys). Typed divergences (never a wrong
  result): `self/0` resolves through the thread-local `SelfHook` seam, default typed `badarg` (no hook ⇒
  a `self/0` guard clause is skipped, a `self/0` body is `badarg`); integer arithmetic leaving the 60-bit
  small range through `DefaultArithHook` is `unsupported_ets`, while `beam_bifs` installs an
  arbitrary-precision hook (`beam_bifs/src/ets/arith_hook.rs`) around every match-spec-evaluating BIF so
  a bignum operand yields the oracle's value in the runtime; a **body** runtime failure yields the atom
  `EXIT` from the caller's `AtomTable` (a host running a raising body must pre-intern `EXIT`; an
  uninterned `EXIT` ⇒ `badarg`).
- **`erlang:match_spec_test/3`'s `trace` dialect is left-open (typed `unsupported`).** The oracle folds
  the trace **run** into the `{ok,Return,Flags,Errors}` shape and needs the trace-only special forms
  (`get_tcw`/`set_tcw`/`message`/`return_trace`/`exception_trace`/`display`/`set_seq_token`/…) — a runner
  subsystem, not a lint-only subset. The **`table`** dialect *is* implemented (native structural lint +
  this crate's compiler); `beam_bifs::match_spec` returns the typed `unsupported_bif` for `trace`. Detail
  and C refs (`erl_db_util.c:6042/1509/6125`) in `src/select/CONTEXT.md`.
- **`select_replace/2`'s key check is the C's static syntactic check** (`db_match_keeps_key`): a body
  that is not syntactically `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}` is `badarg` for the
  whole call, even when nothing matches. Two exotic shapes remain approximated (a call-shaped raw tuple
  inside a `{const, X}` subterm compared against a literal head tuple, and the double-tuple unwrap inside
  a const subterm) — reachable only from hand-written specs, never a wrong result for any spec the
  battery or the oracle probes exercise.
- **`cmp_arith`'s big-integer↔float key comparison is a best-effort `f64` approximation** (a
  `beam_term` gap): `number_as_f64` rounds an out-of-range bignum to `±inf`; every representable
  integer/float pair is exact.

## Handoffs (to other nodes)
Adopted by the `beam_bifs` bridge (verified): the owner-gated `{protection, Access}` setter
(`Table::setopts`/`SetOpt::Access`) and the read-gated `ets:slot/2` primitive; the C-faithful hash-kind
`last`/`prev` (no BIF-side kind branch); and match-spec head maps (the `table` dialect delegates to
`beam_ets::select::compile`). The `auto`/`{debug_hash_fixed_number_of_locks, N}` `write_concurrency`
rendering stays host-side (`TableInfo::write_concurrency` is a `bool`).
- **`beam_bifs` (open):** install `select::expr::set_self_hook` — no crate does, so `self/0` in a spec
  silently under-matches (a guard skips its clause, a body yields `EXIT`) where the oracle returns the
  pid; `BifContext` already carries `self_pid` (see `src/select/CONTEXT.md`).
- **`beam_bifs` (open):** reject `[]` in `ets:match_spec_compile/1` (the oracle `badarg`s; the select
  family legitimately accepts `[]`) and retain/deliver the `ets:give_away/3` gift term
  (`{'ETS-TRANSFER', Tid, FromPid, GiftData}`).
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, `SetInterior`/`OrderedInterior`, `Access`, `Concurrency`, `WriteConcurrency`, `TableOptions` (incl. heir + `decentralized_counters`), `can_read`/`can_write`; tests in `src/kind/tests.rs`
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store/mod.rs` → `TermStore` (table-owned arena + copy/free/relinquish/reuse) — the copy boundary; `src/store/tests.rs` its unit tests
- `src/table/mod.rs` → `Table`/`TableId`/`OwnerId`, constructors, metadata/ownership accessors (incl. the live `access` field + `set_access`), write ops (incl. `release_empty_interior`), access checks, `select_all`
- `src/table/tests.rs` → the table-level unit tests (write ops, metadata, access/protection, ownership/heir, counters, store accounting)
- `src/table/read.rs` → the read/navigation `impl Table` (incl. `is_named_table`, `slot`, `tab2list_rev`, the hash-kind `last`/`prev` aliasing)
- `src/table/rename.rs` → `Table::rename` + `RenameOutcome`
- `src/table/shared.rs` → `SharedTable` (the locked handle) + its unit tests
- `src/table/view/{mod,read,write,term,iter}.rs` → the guard-scoped `TableRead`/`TableWrite`/`TableTerm` views (the aliasing defence) + the view-borrowed traversal API; `view/tests.rs` their unit tests
- `src/table/interior.rs` → the private `pub(crate) trait Interior` + `build_interior`
- `src/table/access.rs` → `TableAccess` (the access-enforcing view)
- `src/table/ownership.rs` → `Heir`/`SetOpt`, the owner-gated `Table::setopts`, `install_heir`/`heir_data`/`copy_heir`/`replace_heir`, the `give_away` semantics + the pid-gap docs
- `src/table/lifecycle.rs` → `OwnerDeath` + `Table::owner_died`
- `src/table/element.rs` → the table-level `update_counter`/`update_element`; `src/table/update.rs` → the counter/element operand machinery (shared with the interiors)
- `src/champ_interior.rs` → `ChampInterior` — the **default `set`** interior
- `src/hash_interior.rs` → `HashInterior` for `bag`/`duplicate_bag` (and opt-in `set`), `with_keypos` key re-anchoring, `Interior::slot` bucket override + `bucket_count()`
- `src/interior_parity.rs` → cross-interior equivalence + default-selection tests; `src/interior_bench.rs` → the interior microbenchmarks (both `#[cfg(test)]`)
- `src/ordered_interior/{mod,sorted,node,tree,iter}.rs` → the B+-tree `ordered_set` interior (conditional separator refresh, single-descent `insert_new`), the sorted-`Vec` alternative, the node arena, the tree ops, the iterator; `tests.rs` their unit tests
- `src/select/{mod,pattern,expr,exec,run,continuation}.rs` → the match-spec compiler, executor, runner and continuations (`MatchContinuation` carries the table id + registered name + spec + direction + a private `Cursor`: `Begin` / `Anchor(key)` / `Position(ordinal)`)
- `src/info.rs` → `TableInfo` + `entries` (the 15-field `ets:info` pairs)
- `src/error.rs` → `badarg`/`unsupported` failure constructors
- `tests/common/mod.rs` → shared integration-test helpers; `tests/common/oracle.rs` → the live-`erl` plumbing
- `tests/ets_*.rs` → public-API integration tests by concern (basic/order/select/bag/info/nav/access/access_audit/set_access/update/replace/continuation/continuation_direction/ownership/fixtable/shared/options/rename/reclamation/reclamation_oracle/memory/copy_oracle/concurrency/slot/store_terms), plus `tests/select.rs` and `tests/match_spec_run.rs`
- `tests/differential.rs` + `tests/differential/{battery/*,erl_program.rs}` → the differential evidence suite vs the system `erl` (191 cases, 15 battery groups)
- `tests/ets_ordered_oracle.rs` → the randomised mixed-type `ordered_set` differential trace (60-slot op space incl. a `next`/`prev` walk + key-boundary dimension) vs the live oracle (`#[ignore]`d; run with `--ignored`)
- `ETS_SURFACE.md` → the `ets:*` coverage census + the oracle-probed semantic facts + the runtime-level gaps and the exact API each needs
- `PERF.md` → the crate's measured memory/perf figures (§3 the `ordered_set` B+-tree-vs-sorted-`Vec` comparison across key orders; §1 the `ets_copy` peak-RSS attribution; §2 per-object reclamation) — both sides of every ratio + `/proc/loadavg`
- `../../erts/emulator/beam/erl_db.c` / `erl_db_hash.c` / `erl_db_tree.c` / `erl_db_util.c` / `erl_db*.h` → the API, interiors, key/match handling and flags being mirrored (read-only)

## See Also
- `ETS_SURFACE.md` → coverage census: every `ets:*` operation mapped to this crate / `beam_bifs` / missing, the oracle-probed facts, and the runtime-level gaps with the exact API each needs.
- `PERF.md` → the crate's measured memory/perf figures (§3: the `ordered_set` B+-tree-vs-sorted-`Vec` comparison at several sizes and key orders; §1: the `ets_copy` peak-RSS attribution; §2: per-object reclamation vs the oracle) — both sides of every ratio + `/proc/loadavg`.
- `../CONTEXT.md` → crate-layer contracts, the `Table` seam, dependency waves, milestone scope.
- `../../CONTEXT.md` → workspace contracts (§1 A.1/A.8, §2 frozen term layout, §3 error strategy, §7).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`hashing` utilities.
- `../beam_alloc/CONTEXT.md` → the `AllocRef`/`AllocType::Ets` allocation seam `TermStore` uses.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §B/§D → data-structure and ETS copy-trimming discussion.
