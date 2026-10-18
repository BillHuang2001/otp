# `beam_ets` — ETS Tables (Stage 3 data structures)

## Intent
Owns the **observable semantics of `ets:*`** — Erlang Term Storage — over **flat, table-owned term storage** (the A.1 "copy boundary"): Part I §A.1 (term/key/value semantics), §A.8 (`ets:info/1` introspection), plus the Stage-3 row of Part II §H — `set` over a **CHAMP** index, `bag`/`duplicate_bag` over a **chained-bucket** hash index, `ordered_set` over a **K-ary B+-tree**.
The **interiors are NOT externally pinned** (crate-layer `../CONTEXT.md` §2): bucket layout, hash-map thresholds, tree fan-out and load factors are choices; only the **semantics** are contract — which object wins on a duplicate key, key equality/ordering, `size`, and what `lookup`/`select`/`tab2list`/`first`/`next` return.
Term **ordering** and **hashing** are owned by `beam_term` (`compare::{cmp,cmp_arith,eq}`, `hashing::term_hash`) and MUST NOT be re-implemented here.
Legacy ground truth (read-only): `../../erts/emulator/beam/{erl_db.c,erl_db_hash.c,erl_db_tree.c,erl_db_util.c,erl_db*.h}` — mirror the SEMANTICS, not the layouts.
Two companion docs carry what this file only points at: **`ETS_SURFACE.md`** (the per-`ets:*`-operation coverage census — this crate / `beam_bifs` / missing, the oracle-probed facts, the runtime gaps and the API each needs) and **`PERF.md`** (every measured figure with both sides of each ratio; §1 the `ets_copy` peak-RSS attribution, §2 per-object reclamation, §3 the `ordered_set` interior comparison).
The crate's own adopt/copy/read path is **flat in memory** (a million same-key upserts keep `TermStore::words`/`blocks` and `Table::memory()` constant), so the `copy_bound/ets_copy` loop's rust-vs-oracle RSS growth is **sibling-side** (`beam_rt/src/context/calls.rs` nursery growth) — `PERF.md` §1.

## Position & dependencies
Crate-layer **wave 1** (`../CONTEXT.md`).
Depends only on `beam_common` (errors), `beam_term` (terms + ordering/hashing), `beam_alloc` (table-owned allocation) and `libc`.
Consumed by `beam_bifs` (the `ets:*` BIFs) and `beam_rt`.
No new dependency edges without reporting to `../`.
The **atom table is a per-call parameter**: every op that compares or hashes a term takes `atoms: &AtomTable` (one live global table owned by `beam_rt`; a `Table` stores no atom state).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_ets::prelude::*`.

### `kind.rs` — kinds, access, options
- `pub enum TableKind { Set, OrderedSet, Bag, DuplicateBag }` (`Default = Set`): `as_atom_name()`, `from_atom_name(name)`, `is_multi()`, `is_ordered()`.
- `pub enum Access { Public, Protected, Private }` (`Default = Protected`): `as_atom_name()`, `from_atom_name(name)`.
- `pub enum Concurrency { None, Read, Write, ReadWrite }`: `as_atom_name()`, `from_atom_name(name)` — **advisory** (parsed/reported, never enforced).
- `pub enum WriteConcurrency { #[default] Off, On, Auto, FixedLocks(u32) }` — the `{write_concurrency,_}` mode whose `Off`/`On` projection is the bool API: `is_enabled()`, `fixed_locks() -> Option<u32>`, `as_atom_name() -> Option<&'static str>` (`"false"`/`"true"`/`"auto"`; `None` for `FixedLocks`), `from_atom_name(name)`; `FixedLocks(n)` keeps the **requested** count.
- `pub enum SetInterior { Chained, #[default] Champ }`, `pub enum OrderedInterior { #[default] BPlusTree, Sorted }` — crate-internal interior selectors (`as_str()`); `Table::with_set_interior`/`with_ordered_interior` install, `set_interior()`/`ordered_interior()` report.
  Not `TableOptions` fields, no BIF surface.
- `pub struct TableOptions` — the builder (defaults `Set`, keypos 1, `Protected`, unnamed, uncompressed, no concurrency/heir/`decentralized_counters`): `new()` plus one setter per field (`set_kind`, `set_keypos(usize) -> Result<&mut Self>` — rejects 0, the only fallible builder, `set_access`, `set_named`, `set_name(Atom)`, `set_compressed`, `set_write_concurrency(bool)`, `set_read_concurrency(bool)`, `set_write_concurrency_mode(WriteConcurrency)`, `set_heir(Option<Heir>)`, `set_decentralized_counters(Option<bool>)`), one getter per field (`kind()`, `keypos()`, `access()`, `is_named()`, `name()`, `is_compressed()`, `write_concurrency()`, `read_concurrency()`, `heir()`) plus `write_concurrency_mode() -> WriteConcurrency`, `decentralized_counters_option() -> Option<bool>`, `decentralized_counters() -> bool`, `concurrency() -> Concurrency`; `impl Default`.
  A `private` table forces the effective `write_concurrency` off; `set_name` records the name only (not `named_table`).
  Tests `src/kind/tests.rs`.
- `pub fn can_read(access, owner, requester) -> bool` / `can_write(…)` — the access matrix (`requester == owner` ⇒ everything).

### `key.rs` — object/key handling (the DB key rules)
- `key_of(object, keypos) -> Result<Eterm>` — an object **must be a tuple** with `arity >= keypos`; non-tuple / short arity / `keypos == 0` ⇒ `BeamError::BadArg`.
  `is_valid_object(object, keypos) -> bool`.
- `pub unsafe fn object_elements<'a>(obj) -> &'a [Eterm]` — the tuple payload slice for `select` predicates (`# Safety`: `obj` must be a live tuple).

### `store/mod.rs` — table-owned term storage (the copy boundary)
- `pub struct TermStore` — a **never-relocating bump arena** over a `beam_alloc::AllocRef` (`AllocType::Ets`) with **per-object reclamation**: `new(alloc)`, `with_default_allocator()`, `copy(root) -> Result<Eterm>`, `free(root) -> usize`, `relinquish(root) -> usize`, `reset()`, `words()`, `blocks()`, `memory()`; `impl Drop` frees every block.
  Deep-copy covers every kind `ets:insert/2` accepts (residual typed `BadArg`s in Known Issues).

### `table/mod.rs` — the `Table` handle + ops
- `pub struct TableId(u64)`: `new(u64)`, `raw()`, `next()` (process-global monotonic), `Display`.
  `pub struct OwnerId(u64)`: `new(u64)`, `raw()`.
- `pub struct Table`: metadata + a table-owned `TermStore` + one interior behind the private `Interior` trait; constructors `new(id, opts)`, `with_allocator(id, opts, alloc)`, `with_set_interior(id, opts, SetInterior)`, `with_ordered_interior(id, opts, OrderedInterior)`.
- **Shape conventions.** Every fallible op returns `beam_common::Result<_>`; `first`/`last`/`next`/`prev` return `Option` (`None` = `'$_end_of_table'`); unless noted an op takes `atoms: &AtomTable`, and the select/match family plus the element updates also take `heap: &mut Heap` (scratch).
- **Metadata:** `id`, `kind`, `name() -> Option<Atom>` (Some iff a name was recorded), `keypos()` (1-based), `access`, `is_named_table()`, `owner()/set_owner()`, `heir()/set_heir()/heir_data()`, `give_away`, `give_away_with_gift -> Result<Transfer>`, `setopts(caller, SetOpt)`, `set_access`, `owner_died -> OwnerDeath`, `info() -> TableInfo`, `rename -> Result<RenameOutcome>`.
- **Writes** (object = the whole tuple; key from `keypos`): `insert` (upsert), `insert_new -> bool`, `insert_list(&[object])` (**ATOMIC**), `delete(key)`, `delete_object(object)`, `delete_all_objects()`, plus the `pub(crate)` `replace_key(key, &[object])` behind a `duplicate_bag` `select_replace/2`.
- **Reads:** `lookup(key) -> Vec<Eterm>` (0/1 for set/ordered_set), `member`, `lookup_element(key, pos)`/`lookup_element_or(.., default)` (scalar; typed `unsupported` for the multi kinds), `lookup_element_all(..)`/`lookup_element_all_or(..)` (their list result), `slot(slot, atoms) -> Result<Option<Vec<Eterm>>>` (0-based; `None` ⇒ `'$_end_of_table'`; out of range ⇒ `badarg`), `memory()` (table-owned store **plus** index storage), `size()` (objects, not keys, for bag/dup_bag), `is_empty()`, `tab2list()` (ordered for ordered_set).
- **Element updates:** `update_counter(key, op)`, `update_counter_with(key, op, default)`, `update_element(key, &[(pos, term)]) -> bool`.
- **Navigation:** `first()`, `last()`, `next(key)`, `prev(key)`, `take(key) -> Vec<Eterm>`, `pub(crate) tab2list_rev()`; hash-kind `last`/`prev` alias `first`/`next`; an unknown anchor is `badarg` (`check_navigable`).
- **Access control:** `check_read`/`check_write`/`as_requester -> TableAccess<'_>`.
  **Select family** (`select/exec.rs`) + `select_all(pred)`: `select`, `select_reverse`, `select_delete -> usize`, `select_count -> usize`, `match_object`, `match_`, `match_delete -> usize`, `select_replace -> usize`, plus the limit forms (`continuation.rs`, each `-> (Vec<Eterm>, Option<MatchContinuation>)`): `select_limit`/`select_continue`/`select_reverse_limit`/`select_reverse_continue`/`match_limit`/`match_continue`/`match_object_limit`/`match_object_continue`.
- `Table::set_fixation(bool)`/`is_fixed()`/`deferred_objects()` — `ets:safe_fixtable/2`; `pub fn match_spec_run(list, spec, atoms, heap) -> Result<Vec<Eterm>>` (`run.rs`).
- `pub(crate) trait Interior` — the dispatch seam (`insert`/`insert_new`/`delete`/`delete_object`/`lookup`/`member`/`lookup_element(+s)`/`size`/`iter_objects(+_rev)`/`keys`/`take`/`slot`/`first_key`/`last_key`/`next_key`/`prev_key`/`memory`/`update_counter`/`update_element`), whose `insert` returns `Result<bool>` — **whether the object was retained** — so `Table::insert` reclaims the copy it made when the interior rejects it.
  Per-file signatures: `table/CONTEXT.md`.

### `table/rename.rs` — `ets:rename/2` (oracle-pinned)
- `pub fn Table::rename(&mut self, name: Atom, is_taken: impl FnOnce(Atom) -> bool) -> Result<RenameOutcome>`, `pub enum RenameOutcome { Named { previous_name: Option<Atom> }, Unnamed }`.
  Only the recorded name changes, so objects, `TableId`, owner, access, kind and keypos survive by construction.
  With `named_table` an already-registered new name (including its own) is `badarg` and the BIF returns the **new name atom**; **without** `named_table` the rename always succeeds and the BIF returns the **unchanged table identifier**.
  The registry rekey, the write gate and the non-atom-name/unknown-table `badarg`s are the host's (`ETS_SURFACE.md` §1).

### `table/shared.rs` + `table/view/**` — the shared handle and its guard-scoped views
- `pub struct SharedTable` (`Arc<RwLock<Table>>`): `new`, `id`, `ref_count`, `read() -> Result<TableRead<'_>>`, `write() -> Result<TableWrite<'_>>`, `with_read(|v: &mut TableRead<'_>| …)`, `with_write(…)`, `try_into_inner -> Result<Option<Table>>`, `Clone`, `Debug`.
  A poisoned lock yields the **typed** `unsupported_ets: table lock poisoned`.
- The read/write surface is a **lifetime-tagged, guard-scoped view**, so a term read through it cannot outlive its guard (a compile error, pinned by `compile_fail` doctests): `TableTerm<'g>` — a **private** `Eterm` + `PhantomData<&'g Table>` with **no** method returning a bare `Eterm`, carrying the `is_*`/`as_*`/`tuple_*`/`size` accessors, `eq_term`/`cmp_term`, and the copy-out `copy_into(&mut TermStore) -> Result<Eterm>` (the only way to keep a term past the guard).
- `TableRead<'g>` — owned metadata (`id/size/is_empty/memory/info/kind/keypos/access/owner/name/named_table/is_fixed`), term reads returning `TableTerm<'g>` (`member`, `lookup`, `lookup_element(+_or/_all/_all_or)`, `first`, `last`, `next`, `prev`, `tab2list`, `slot`), and the **copy-out** select family (`select`, `select_count`, `match_object`, `match_` take a `&mut TermStore` destination).
- `TableWrite<'g>` — metadata, the mutation family over caller-owned inputs, `heir() -> Option<HeirView<'_>>`, `take -> Vec<TableTerm<'_>>`, and `as_requester` (the raw access seam whose reads are bare aliasing `Eterm`s by design).
  `HeirView<'g>` (re-exported at the crate root): `owner() -> OwnerId`, `data() -> Option<TableTerm<'g>>`.
- **View-borrowed traversal** (`view/iter.rs`) — `TableRead::{iter_keys, iter_keys_rev, iter_objects, iter_objects_rev}` (each yields `TableTerm<'a>` bound to the view borrow via `ets:first|next`/`last|prev`; the `*_objects` forms expand the multi kinds per object), `TableRead::{foldl, foldr}`, and the bulk copy-out `TableRead::tab2list_into(&mut TermStore) -> Result<Vec<Eterm>>`.
  Iterators are `impl Iterator` (no private type leaks); the views are `!Send` (scratch `Heap`).
  Per-method detail: `view/CONTEXT.md`.

### `table/ownership.rs` — `ets:give_away/3` + `{heir, …}` + `{protection, …}` + the heir data
- `pub struct Heir { pub owner: OwnerId, pub data: Option<Eterm> }` — `Some(term)` = `{heir, Pid, Data}`, `None` = `{heir, Pid}` (the C `THE_NON_VALUE`).
  `pub enum SetOpt { Heir(Heir), ClearHeir, Access(Access) }` is the parsed `ets:setopts/2` option (the subset modelled); `Table::setopts(caller, SetOpt) -> Result<()>` applies the **owner gate first** (`caller != owner` ⇒ `badarg`, nothing mutated), and `give_away` follows the C `ets_give_away_3` order (bad-pid → not-owner → self-gift, all `badarg`, owner unchanged on failure).
- `pub struct Transfer { pub to: OwnerId, pub from: Option<OwnerId>, pub data: Eterm }` — the `{'ETS-TRANSFER', Tid, FromPid, GiftData}` **delivery obligation** a gift-carrying `ets:give_away/3` returns (`data` = a table-owned checked-out gift copy; the `Tid`/`FromPid` *terms* are the host's to build — the pid gap).
- `Table::give_away_with_gift(caller, new_owner, gift) -> Result<Transfer>` — the same C-ordered ownership check (shared private `transfer_target`) **then** a deep copy of `gift` through `TermStore::copy`, checked out with `TermStore::relinquish` like `Table::take`.
  `TableAccess`/`TableWrite` mirror it.
- `Table::install_heir(owner, data: Option<Eterm>) -> Result<()>` **copies** the gift data into table-owned storage (a non-copyable term is the typed `badarg`, heir untouched); `Table::heir_data() -> Option<Eterm>` reads it back.
  `Table::copy_heir` is the shared `pub(super)` copy seam (`Table::build` + `install_heir`); `Table::replace_heir` is the shared `pub(super)` reclaim seam every overwrite site funnels through, so the outgoing heir's copy is freed exactly once.
  `set_heir`/`setopts` stay owner-level.
- **`Table::set_access(&mut self, Access)`** is the post-creation protection setter (the owner-level primitive behind `setopts(_, SetOpt::Access(_))`): `Table` owns the live `access` field (from `opts.access()`) and `access()`, `info().access`, `check_read` and `check_write` all read **that field**.
  `TableAccess::set_access` is the enforcing form; `TableWrite::set_access` is the guard-scoped mirror.
  **`TableOptions` is NEVER mutated.**
- **The pid gap.** No pid term and no process-liveness concept: a `None` new owner stands in for a non-pid/dead pid target, an unknown table id is the registry/BIF layer's concern, and a set heir/owner renders as `undefined`.

### `table/lifecycle.rs` — the owner-death transition
- `pub enum OwnerDeath { NotOwner, Deleted, Transferred(Heir) }` + `Table::owner_died(dead, is_alive)`.
  `NotOwner` is a complete no-op; `Deleted` (no heir, a self-heir, or a heir the liveness oracle rejects) obliges the runtime to drop the table; `Transferred(heir)` sets the owner to `heir.owner`, **keeps the heir installed**, and obliges the runtime to deliver `{'ETS-TRANSFER', Tid, dead_pid, data}` — **only when `heir.data` is `Some(data)`** — with `Tid` the registered name for a `named_table`, else the table id.
  Neither effect is this crate's job.

### `table/access.rs` — the access-enforcing view
- `pub struct TableAccess<'a>` (from `Table::as_requester`) — rechecks `can_read`/`can_write` on every op and raises `BeamError::BadArg` on a violation.
  Write-checked: the write family (`insert`/`insert_list`/`insert_new`/`delete`/`delete_object`/`delete_all_objects`/`take`/`update_counter(+_with)`/`update_element`/`select_delete`/`match_delete`/`select_replace`/`rename`); read-checked: the read family (`lookup`/`member`/`lookup_element*`/`select*`(+_limit/_continue)/`match_object`/`match_`/`first`/`last`/`next`/`prev`/`tab2list`/`slot`, plus `set_fixation` — the oracle gates `safe_fixtable/2` as a READ) and the limited forms.
- `ets:match_spec_run/2` has no access concept; `ets:info/1,2` is deliberately **not** gated; `give_away`/`give_away_with_gift` have no protection check (ownership is not a read/write op); `setopts`/`set_access` are owner-gated.

### `table/update.rs` — `update_counter`/`update_element` operand machinery
- `pub(crate)` helpers shared with the hash interior: `counter_slot`, `add_counters`, `replace_elements` (re-exported from `table/mod.rs`), plus the decode/build internals (`decode_counter_ops`/`decode_counter_tuple`/`counter_operand`/`CounterOp`/…).

### `champ_interior.rs` — CHAMP open-addressing interior (**default `set`**)
- `pub struct ChampInterior` + `Default`/`new()`: each level consumes `WIDTH = 5` bits of the 64-bit `term_hash`, compressing children into a `datamap` (inline slots) + `nodemap` (sub-nodes); a full-hash collision lives in a `CollisionNode` (`eq`-compared), so a pathological hash cannot recurse past 64 bits.
  Every `Interior` method is overridden; `last_key`/`prev_key`/`iter_objects_rev` apply the hash-kind aliasing.
  **Delete canonicalization** is bottom-up: a sub-node holding exactly one entry is **inlined** into its parent as an inline slot (a sub-node is indexed by the *next* 5 hash bits, so promoting a multi-entry sub-node wholesale would break routing).
  A removal also gives capacity back (`trim_capacities`).

### `hash_interior/` — chained hash-index interior (`bag`/`duplicate_bag`; opt-in `set`)
- `mod.rs` (587) — `pub struct HashInterior`: buckets `Vec<Vec<Entry>>` (`Entry { key, objs: Vec<Eterm> }`), lookup by `term_hash(key) % buckets` then `eq`; `set` keeps ≤ 1 object per key, `bag` rejects a duplicate *object* within a key (its `insert` returns `false`, so the caller reclaims the rejected copy), `duplicate_bag` keeps a multiset; `delete_object` removes **every** equal object (the oracle).
  Public API: `new(kind)`, `with_keypos(kind, keypos)`, `take`, `update_counter`, `update_element`, `memory(atoms)`, `bucket_count()` (the `nactive` boundary), `keys()`, plus the `Interior` impl incl. the `slot` bucket override.
- `capacity.rs` (263) — the bucket array's **growth and release policy** (a named `mod` with its own docs, no `unsafe`): the load-factor constants, the rebuild-at-exact-capacity `rehash_into`, and the `pub(super)` `maybe_grow`/`maybe_shrink`/`trim_objs`/`initial_buckets()` (see Design Decisions).
- `tests.rs` (571) — the interior's unit tests; the shrink/churn ones live in `capacity.rs`, next to the policy they pin.

### `ordered_interior/` — the ordered `ordered_set` interiors (B+-tree + sorted `Vec`)
- `mod.rs` — `OrderedSetInterior` (K-ary B+-tree): `new()`, `first`/`last`, `first_key`/`last_key`, `next`/`prev`, `next_from(bound, inclusive)`/`prev_from(bound, inclusive)`, all via `beam_term::compare::cmp_arith` (`erl_db_tree.c` `cmp_key_eq`, `CMP = erts_cmp(a, b, 0, 0)`), so `1` and `1.0` are the same key; whole-object identity (`delete_object/2`) stays the exact `compare::eq`.
  Plus its `Interior` impl; the node arena **is** counted by `memory()`, and it is released as keys leave (compaction + per-leaf trim), so the figure follows the live key count.
- `sorted.rs` — `SortedOrderedInterior`: the same semantics over one sorted `Vec` (O(log n) lookup via the shared `node::search_entries`, O(n) insert/remove), same navigation, `memory()` measured at capacity but given back through `release_capacity` (`CAPACITY_SHRINK_FACTOR = 4`) as entries leave; selectable via `OrderedInterior::Sorted`.
- `node.rs` — the arena node layout, `ORDER`/`MIN_OCCUPANCY`, the `NodeStore` (`NodeId` slots + free list; nodes never relocate), the shared `Entry` record, `search_entries`, `words_of`, and the capacity-release pair: `needs_compaction()` (`retired.len() > live`) and `compact(&mut root)`, a dense arena renumbering that relocates every `leaf.next`/`branch.children`/the root and `trim_vec`s each surviving node before dropping the retired slots.
- `tree.rs` — insert/lookup/delete with split + borrow-or-merge rebalance, the replace-key-term rule, the separator invariant `keys[i] == max(child[i])`; `delete` runs `NodeStore::compact` after `collapse_root()` and every leaf removal trims that leaf's own entry vector (a single-leaf tree retires no node, so nothing else would revisit it).
  `iter.rs` — the leaf-chain ordered iterator.

### `select/` — the match-spec compiler + executor
- `mod.rs` — `pub fn compile(spec, atoms) -> Result<MatchSpec>`; `MatchSpec { clauses }`, `MatchClause { head, guard, body }`, `pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a` (the `Table::select_all` primitive); legacy aliases `MatchPattern`/`MatchElement` = `Pattern`, `MatchGuard`/`MatchBody`/`MatchBodyElement` = `Expr`.
- `pattern.rs` — `Pattern`: nested tuple/list/improper patterns, quoted literals, `'_'`, `'$N'` binds with repeat-occurrence equality; every other atom (including `'$_'`/`'$$'`, special only in a *body*) is an exact `=:=` literal.
- `expr.rs` — `Expr`/`CmpOp`/`TypeTest`/`ArithOp`, the guard/body language: the connectives (`andalso`/`and`/`orelse`/`or`/`not`), the comparisons (`=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`), the type tests (`is_atom`/`is_integer`/`is_float`/`is_number`/`is_tuple`/`is_list`/`is_binary`/`is_bitstring`/`is_map`/`is_pid`/`is_port`/`is_reference`/`is_function`/`is_boolean`), the guard BIFs (`element/2`, `abs/1`, `size/1`, `hd/1`, `tl/1`, `length/1`, `map_get/2`, `is_map_key/2`, `map_size/1`, `tuple_size/1`, `byte_size/1`, `bit_size/1`, `float/1`, `trunc/1`, `round/1`, `ceil/1`, `floor/1`, `bnot/1`) and `self/0` through the thread-local `SelfHook` seam (default typed `badarg`).
- Map construction works in guards and bodies; map/tuple/binary operands are read through `beam_term`.
  Arithmetic (`+ - * / div rem band bor bxor bsl bsr`, unary `+`/`-`) routes through the `pub trait ArithHook` seam (`DefaultArithHook` + `set_arith_hook`, a `thread_local`) — no crate edge to `beam_bifs`.
  Bodies: sequences (value = last expression), tuple/list/map construction, literals, `'$N'`, `'$_'`, `'$$'`.
  Guard truth is an internal `Expr::Bool` (independent of the atom table); anything outside the recognised set ⇒ `compile`/`expr` `badarg`.
  Capability census: `select/CONTEXT.md`.
- `exec.rs` — `impl Table`'s select family + `pub enum BodyKind` (`WholeObject`/`AllBindings`/`LiteralTrue`) reusing the compiler for `ets:match*`; `guards_hold -> Result<bool>` (a guard the oracle would raise, or a non-boolean one, skips the clause; a capability gap propagates as a typed error).
  `run.rs` — `match_spec_run(list, spec, atoms, heap)`.
- `continuation.rs` — `pub struct MatchContinuation` (opaque: table id + the `named_table`'s registered-name atom (else `None`) + the compiled spec + body + limit + direction + a private `Cursor`) and the limited forms.
  `Cursor` = `Begin` (a `/3` start) / `Anchor(key)` (an `ordered_set` resume) / `Position(ordinal)` (a hash kind's resume ordinal); `resume` rejects a mismatched table id **or** recorded name and re-applies the stored direction and spec.

### `info.rs` — introspection (`ets:info`)
- `pub struct TableInfo { id, name: Option<Atom>, kind, keypos, access, size, memory, owner: Option<OwnerId>, heir: Option<OwnerId>, named_table, compressed, write_concurrency, read_concurrency, decentralized_counters }`; `entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)>` — the heap-free `{Key, Value}` pairs of the oracle's **15** `ets:info/1` fields in the oracle's order (`id`, `decentralized_counters`, `read_concurrency`, `write_concurrency`, `compressed`, `memory`, `owner`, `heir`, `name`, `size`, `node`, `named_table`, `type`, `keypos`, `protection`).
  The `beam_bifs` bridge builds the observable proplist (same order).

### `error.rs` — failure constructors
- `pub(crate) fn badarg() -> BeamError`; `pub(crate) fn unsupported(what: &str) -> BeamError` (a `BeamError::Raise`, error class, reason atom `unsupported_ets: <what>`).
  No crate-local error enum.

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`.
  `badarg`-style failures ⇒ `BeamError::BadArg`; never `panic!`/`unwrap()` on runtime data.
  Table ops never panic on malformed input.
- **Never re-implement term order/hash** (`beam_term::compare::{cmp,cmp_arith,eq}`, `beam_term::hashing::term_hash`) — workspace Prevent-Duplication rule.
- **Table-owned storage only (A.1 copy boundary).** An inserted object is **deep-copied** into the table's `TermStore`; the table never retains a pointer into a caller's heap.
  Objects/keys returned from reads alias table storage and are valid for the table's lifetime (see Known Issues).
- **`unsafe` confinement.** Only `store/`, the tuple accessor in `key.rs`, the small term-reading blocks in `select/`, `table/mod.rs` (`select_all`) and `hash_interior/mod.rs` (the destructive counter-element write), plus the `unsafe impl Send/Sync` on `TermStore`; `hash_interior/capacity.rs` and `hash_interior/tests.rs` have none.
  Every `unsafe fn` has a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale (`beam_common::safety`).
- **Files ~1000 lines**, one responsibility per file.
  `src/hash_interior/` is the split example: `mod.rs` (587) type/ops/`Interior` impl, `capacity.rs` (263) the bucket growth/release policy, `tests.rs` (571) unit tests.
  The only `src/` files over budget are unit-test siblings (`store/tests.rs` 1550, `ordered_interior/tests.rs` 1233, `table/view/tests.rs` 1105) and `select/expr/mod.rs` (1054); large `tests/` modules are legitimate (`tests/differential/erl_program.rs`, `tests/select.rs`).
- **Licensing** Apache-2.0; no new external deps (only `beam_common`/`beam_term`/`beam_alloc`/`libc`).

## Design Decisions
- **The shared handle is guard-scoped, not copy-on-read.** `TableRead`/`TableWrite` hold the `RwLock` guard and every term they yield is a `TableTerm<'g>` bound to the view's borrow, so holding one past the guard is a **compile error** (six `compile_fail` doctests), not a caveat; the default path allocates nothing (only `TableTerm::copy_into`/the copy-out `select` forms do).
- **Heir gift data is table-owned; arity 2 is representable.** `Heir.data: Option<Eterm>` distinguishes `{heir, Pid}` from `{heir, Pid, undefined}`; the data is deep-copied at creation and at `install_heir`, and reclaimed by `replace_heir` at every overwrite site (the C `ERTS_ALC_T_DB_HEIR_DATA`/`free_heir_data` pair) — the caller's term would dangle once its heap died.
- **`ordered_set` defaults to a K-ary B+-tree** (`ORDER = 32`, `MIN_OCCUPANCY = 16`) over a non-relocating arena `NodeStore` with a leaf chain and `O(log n)` point lookup; keys compare by `cmp_arith`.
  Insert is `O(log n)` node visits amortized (worst case `O(log² n)` on a cascading root split), delete `O(log² n)` visits but only `O(log n)` term comparisons.
  The sorted-`Vec` interior stays selectable as a comparison index; `PERF.md` §3 settles the choice in the tree's favour.
- **`delete_rec` rebalances eagerly on the unwind, so the tree never carries sparsity.** A child that fell below `MIN_OCCUPANCY` borrows one entry (leaf) or child (branch) from a sibling holding `> MIN_OCCUPANCY` — left preferred — and only else merges with that sibling and drops a separator, which can under-fill the parent in turn and cascade upward; `collapse_root` handles the root.
  `NodeStore::compact` is **not** the rebalance: it reclaims arena **slots** plus spare `Vec` capacity only, so an arena report like `slots=19, retired=18, live=1` after a `10 000`→1 teardown is the expected free-list state, not leftover tree sparsity.
  The invariant is pinned twice — `Tree::check_invariants`' density half (a multi-leaf chain is at most `len / MIN_OCCUPANCY` leaves) and `src/ordered_interior/tests.rs::assert_occupancy_floor`.
- **`set` defaults to CHAMP; `bag`/`duplicate_bag` stay chained**, over a private `Interior` trait rather than a pinned layout (generic rebuild-on-heap defaults, in-place destructive hash overrides, so the B+-tree needs no writable index slot).
  Measurement-driven, index only: CHAMP wins on insert, iteration and index bytes/entry, ties on lookup-hit, loses on delete — the insert/iterate/memory profile ETS is optimised for.
  `Chained` stays selectable for `set`; `src/interior_parity.rs` proves the traces identical.
- **One `TermStore` per table, copy-on-insert, per-object free.** `free(root)` returns a removed root's exact-word-size runs to per-size free lists that `copy` reuses before bumping, and is a no-op for a root the store never tracked (which makes the heir reclaim path safe).
- **`take` hands back checked-out copies and reclaims the stored originals.** Copy-before-free is mandatory (freeing first would let a later same-size `copy` reuse the words the caller holds); each copy is `relinquish`ed — valid for the table's lifetime, never handed out again, no longer counted toward `memory()`.
- **Every interior releases index capacity as occupancy falls,** so `ets:info(T, memory)` tracks the **live** key/object count rather than a bulk teardown's high-water mark.
  Each home keeps its own hysteresis, so a stable-size churn never reallocates: `hash_interior/capacity.rs`'s `maybe_shrink` rebuilds the bucket array through `rehash_into` once the load falls below 1/8 (a 6× band against the 3/4 grow threshold) and `trim_objs` drops an entry's `objs` slack past 4×; `champ_interior`'s `trim_capacities` releases a slot vector once `capacity > 8 && capacity > 4 * len`; `ordered_interior`'s `Tree::delete` compacts the arena when `needs_compaction()` plus the per-leaf trim; `sorted.rs`'s `release_capacity` (`CAPACITY_SHRINK_FACTOR = 4`) fires from `delete`/`delete_object`.
  The **contract is the shape**, not a word count: memory falls with live data, a 200-round stable-size churn neither grows nor shrink/re-grows, and the empty floor is exact — `release_empty_interior` rebuilds an emptied interior through `build_interior`, because a downsized index is not a *fresh* one (and a removal that did not happen never churns the index).
  Word counts are this crate's own (`../CONTEXT.md` §2); trajectories are in `PERF.md` §2 and pinned by `tests/ets_capacity.rs`.
  `ets:safe_fixtable/2` is the one deliberate exception — removed storage waits in `deferred_frees`, so memory falls at unfix.
- **An interior's stored key term aliases a live, table-owned object.** `key_of(object, keypos)` returns a pointer *into* the object's `TermStore` storage, so an interior keeping a key must re-point it whenever it replaces or removes the object it aliased: on a `set` upsert and, for `bag`/`duplicate_bag`, when `delete_object` removes the object whose key element it pointed at.
  `Table::relocate_rebuilt_object` derives the index key from the **table-owned copy**, never the caller's key (`tests/ets_reclamation.rs`).
- **Access and ownership are `Table`-level, not access-mode concerns.** `can_read`/`can_write` mirror `db_check_access` (`requester == owner` ⇒ full access); `TableAccess` enforces while the raw `Table` ops stay owner-level; `give_away/3` and `setopts/2` never consult `protection` (they are owner-gated).
- **`ets:info/1` mirrors the oracle's 15 fields in the oracle's order** (the reverse of the C `fields[]`, `erl_db.c:4406`); `heir`/`node` are runtime-shape constants and `decentralized_counters` resolves to effective `write_concurrency` **and** the explicit `{decentralized_counters, Bool}` when given, else `kind.is_ordered()` (the CATREE default).
  `private` normalizes the concurrency flags off, so it is false too.
- **`insert_list` is atomic; `select_replace/2` is a two-phase rewrite with the C's static body check** — a body outside `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}` is `badarg` for the whole call even with no match (table unchanged); a valid body applies atomically.
  A `duplicate_bag` replacement **rebuilds the affected key** (it must land on the **specific** matched object): the objects come from `Table::lookup`, each replacement is computed on the caller's scratch heap, and `Table::replace_key` deep-copies the new list into table storage **first**, then frees the old.
- **Match-spec arithmetic routes through the `ArithHook` seam** (a `thread_local`), keeping wave-1 `beam_ets` free of a `beam_bifs` dependency (candidate to lift into `beam_term`).
  **Continuations are opaque owned values, not terms** (table id + spec + limit + position), so a resume needs no re-compile; `Option::None` is `'$end_of_table'`.
- **`ets:slot/2` is one contract over two decompositions.** The defaulted `Interior::slot` is a **one-object-per-slot** partition — right for both `ordered_set` interiors **and** the CHAMP `set`; `HashInterior` overrides it with its bucket array (`nactive` = bucket count).
  An `ordered_set` slot is **exact** (the (n+1)-th smallest key, `erl_db_tree.c:1139`); a hash slot is a bucket whose count and contents are the interior's own, so only the partition / single end-of-table slot / `badarg` boundary is contract.
- **A BODY runtime failure yields the atom `EXIT`; a GUARD failure skips the clause** (a guard that would raise or is non-boolean skips it; a capability gap propagates as a typed error).
  Boolean *bodies* still render as the interned `true`/`false`.
  `match_delete` returns the deleted count, not the BIF's `true`; `size` counts objects, not keys; a `Table` cannot own atom state (indices are global); the registered `name` and the `named_table` flag are independent.
- **Hash-kind navigation order is unspecified (a deliberate freedom).** Every key a walk yields is present, `first`+`next` is a permutation of the stored keys, and `next`/`prev` on an absent anchor are `badarg`; `ordered_set` navigation is exact (ascending arithmetic key order; absent keys read as bounds).
- **B+-tree branch separators are re-materialised from the rightmost live leaf, but only where the subtree max actually moves.** `Tree::insert_rec` returns an `InsertStep` carrying the ascending path's new maximum, so a separator is rewritten only when the subtree maximum it names changed (or a split creates a new one).
  A replaced separator must be re-materialised (`NodeStore::subtree_max_key`): an upsert reclaims the object that established the subtree maximum, so a stored separator term would dangle even when the key *value* is unchanged.
  The descent **binary-searches** the strictly ascending separators (`O(log ORDER)`, ≤5 `cmp_arith`); `Tree::check_invariants` asserts separator **term identity**.
- **`insert_new` is a single descent.** The same `insert_rec` runs with a refuse-on-existing-key flag, stopping with `InsertStep::Refused` and mutating nothing, so `insert_new/2` never does a separate `find` before its `insert`.
- **`{write_concurrency,_}` is a mode, not a bool.** `WriteConcurrency` (`Off`/`On`/`Auto`/`FixedLocks`) is the model, the bool API its `Off`/`On` projection, so `beam_bifs` stays source-compatible.
  The fine-locking normalization is a **creation-time** property of `TableOptions`: a runtime `set_access` never re-normalizes it (the oracle agrees).
- **The hash kinds' `last`/`prev` ARE their `first`/`next`.** ERTS installs the same function twice (`erl_db_hash.c:853-856`) and `db_select_reverse_hash` delegates to `db_select_hash`'s chunk function (`:2310-2313`), so for `set`/`bag`/`duplicate_bag` there is **no** backward walk: `last/1` == `first/1`, `prev/2` == `next/2` (an unknown anchor stays `badarg`), and `select_reverse` returns exactly what `select` returns.
  `ChampInterior`/`HashInterior` override `last_key`/`prev_key`/`iter_objects_rev`; the trait defaults stay the `ordered_set` semantics.
- **Select-family continuations are key-anchored on an `ordered_set`, ordinal on the hash kinds.** `Cursor::{Begin, Anchor(Eterm), Position(usize)}` is chosen by the table kind, so the shapes never mix.
  The ordered kind resumes from the **last key its chunk yielded** (`Anchor`, an **exclusive** bound), surviving the anchor's own deletion; the hash kinds keep `Position` because their ERTS cursor is a bucket/index, not a key.
  Identity is the table id + the `named_table` name atom; the read gate precedes consumption.

## Concurrency, fixation & sharing (current state)
- **`Table` is `Send + Sync` and shareable across processes**: plain `Eterm` data + a `Box<dyn Interior>` (`Interior: Send + Sync`) + a `TermStore` (`unsafe impl Send/Sync`, rationale: no interior mutability, every storage-reaching method takes `&mut self`).
- **`SharedTable`'s coarse lock is the whole story — no lost updates.** One `write()` guard spans a whole check-then-act sequence (`insert_new`, `update_counter`, `select_replace`), so concurrent writers lose nothing; fine-grained `{write_concurrency}` affects throughput only, never results.
  A poisoned lock is the typed `unsupported_ets: table lock poisoned`.
- **`ets:safe_fixtable/2` fixation = deferred reclamation.** While fixed, every removed object's storage goes onto `deferred_frees` (and `delete_all_objects` defers each root instead of `reset()`ing); unfixing flushes them, so `ets:info(T, memory)` holds while fixed and falls at unfix.
  `deferred_frees` is not counted in `memory()`.
- **Subterm sharing (the copy-boundary trim).** `TermStore::copy_term` shares exactly the one literal whose liveness the crate can prove — the global empty tuple (`copy.c:130-140`); `beam_term::literal::LiteralArea` literals are deep-copied (the area may be dropped and the crate holds no handle).
  `{keypos, N}` keys live inside the stored tuple; `compressed` is parsed/reported with no storage effect.

## Test Strategy
- **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets --offline --no-fail-fast` — **646 passed / 0 failed / 1 ignored across all binaries (incl. 10 doctests, 6 `compile_fail`)**.
  The ignored test is the randomised `ordered_set` differential, which must pass under `--ignored`.
- **Unit tests** per module (`#[cfg(test)]`; big siblings `store/tests.rs`, `kind/tests.rs`, `table/tests.rs`, `table/view/tests.rs`, `ordered_interior/tests.rs`, `hash_interior/{tests,capacity}.rs`, plus the `interior_parity.rs`/`interior_bench.rs` pair): kind/options, key validation, store copy/free/reuse/relinquish, table insert/take accounting, hash-interior ops + shrink policy, CHAMP ops (round-trip, churn, collision node, canonicalization, memory), B+-tree invariants incl. 10 000-key churn, the sorted-`Vec` interior, select compile/execute, info, owner-death, the heir forms/reclaim, the `duplicate_bag` `select_replace` floor, and the guard-scoped views.
- **Cross-interior equivalence + defaults** (`interior_parity.rs`): a shared LCG script drives chained vs CHAMP (identical traces) and B+-tree vs sorted `Vec` (order-sensitive tail), pins the default selectors and the `cmp`→`cmp_arith` arithmetic-key behaviour.
- **Interior microbenchmarks** (`interior_bench.rs`): `… -p beam_ets --lib interior_microbenchmark -- --nocapture` (or `ordered_interior_microbenchmark`), sizes via `BEAM_ETS_BENCH_SIZES`/`BEAM_ETS_BENCH_SHAPES` and key order via `BEAM_ETS_BENCH_ORDER=shuffled|ascending|descending|all`.
  Every cell reports timer-free `words`/`B/entry` and the run asserts ≤ 64 words/entry, so a per-insert memory regression fails the bench (outcomes in `PERF.md` §3).
- **Integration suites over the public API** (`tests/ets_*.rs`, `tests/select.rs`, `tests/match_spec_run.rs`; helpers `tests/common/`, live-`erl` plumbing `tests/common/oracle.rs`).
  Per-file detail: `tests/CONTEXT.md`.
  - *Accounting/reclamation*: `ets_memory.rs`/`ets_copy_oracle.rs` (both sides of the `ets_copy` loop), `ets_reclamation{,_oracle}.rs`, `ets_capacity.rs` (its live-oracle check asserts direction/magnitude only), `ets_info.rs`, `ets_store_terms.rs` (every copyable boxed kind vs the oracle's `phash2`), `ets_replace.rs`.
  - *Access enforcement, per operation, against the live oracle*: `ets_access.rs` (every read/write/`ets:info` field/ownership op/four continuation resumes × owner|non-owner × `public`/`protected`/`private`, against the oracle's `{SizeBefore,SizeAfter,is_list(ets:info(T))}` intact triple); `ets_access_audit.rs` (no divergence; the corners the matrix misses); `ets_access_rights.rs` (the give-away rights matrix, gate-flip timing, and that a **denied continuation resume is not consumed**; whole-table `ets:delete/1` is `HOST_ONLY_DELETE`); `ets_access_options.rs` (the `ets:new/2` option matrix and the one documented `write_concurrency` rendering difference).
  - *Behavioural*: the remaining `ets_*.rs` files by concern (basic/order/select/bag/nav/update/fixtable/slot/shared/options/rename/set_access/continuation{,_direction}/concurrency{,_locks}/empty_tuple_expr); `ets_nav.rs`/`ets_fixtable.rs` also carry a deterministic always-on `first`+`next`/`last`+`prev` walk over 100 keys interleaved with mutation.
- **Differential evidence suite** (the compatibility gate): `tests/differential.rs` runs **191 cases** (15 case-group files under `tests/differential/battery/`, the shared harness `battery/mod.rs`, the embedded oracle program `erl_program.rs`) identically under the system `erl` (OTP 29.1.1 / erts-17.1) and under `beam_ets`, comparing canonical renderings line-by-line — **every case agrees**; `KNOWN_DIVERGENCES` is empty.
  The oracle is **required by default** — a genuinely absent `erl` FAILS the cross-check (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out with a loud skip; `BEAM_ETS_REQUIRE_ORACLE=1` forces it).
- **Randomised `ordered_set` differential** (`tests/ets_ordered_oracle.rs`, `#[ignore]`d): a 73-term mixed-type key pool driven by a SplitMix64-finalised LCG through a **60-slot** op space (34 `insert` slots, one per remaining API op, a `next`/`prev` walk dimension, and a key-boundary `insert_block` longer than `ORDER = 32` so every firing forces a split), comparing `{size, in-order tab2list, op result}` after every step against the live oracle — 1800 steps / 3 seeds agree, **zero divergence**.
  It asserts its own peak and that every slot fired, so an op-mix change cannot silently stop exercising splits, merge-on-delete churn or a walk across a split boundary.

## Known Issues (honest gaps — not full parity)
- **The raw `Table`/`TableAccess` read family still returns bare `Eterm`s that alias table storage** — a caller can hold one past its borrow (or past `take`, or past the table) and then observe words reclaimed by a later same-size insert.
  Inside the tagged views the one exception is `TableWrite::as_requester`, whose READ results are bare aliasing `Eterm`s **by design** (`view/tests.rs::as_requester_reads_are_a_documented_residual_hazard`).
  The fix is a cross-crate decision (`ETS_SURFACE.md` §7.6 names every method that should move behind the tagged-term discipline).
- **`Concurrency` is advisory.** `read_concurrency`/`write_concurrency` are parsed and reported but there is no fine-grained locking; the only synchronisation is `SharedTable`'s coarse `Arc<RwLock>`.
  `{write_concurrency, auto}` and `{debug_hash_fixed_number_of_locks, N}` **are** representable (`WriteConcurrency::Auto`/`FixedLocks(n)`); only the *rendering* stays host-side.
  Fine-grained locking is a **throughput** workstream, never a correctness gap.
- **Owner death is driven by the runtime, for named and unnamed tables alike.** `beam_rt::runtime::ets_owner::drain_ets_owner_exits` (over the `ProcessTable`'s `EtsExitObserver`) calls `Table::owner_died`; `beam_bifs`'s `ets:new/2` records the owner, and the roster is `TableRegistry::handles_owned_by` (**unnamed** tables included) with the `OwnerIndex` unioned in for the registered name a `named_table`'s drop releases (`ETS_SURFACE.md` §7.4).
  **Ownership modelling has no pid/liveness concept**: `give_away/3` treats `None` as the oracle's non-pid/dead-pid target (both `badarg`); delivering the transfer message and dropping the table stay the runtime's job.
- **`TableInfo::entries` renders `owner` as `undefined` and `id` as a small integer** — the crate's own 15-field proplist has no pid/ref term, so the `beam_bifs` `ets:info/1,2` bridge substitutes the recorded owner pid and the tid reference; the BIF output matches the oracle.
- **`ets:setopts/2` is modelled for `{heir, …}` and `{protection, Access}` only.** Every other option (`safe_fixtable`, `{write_concurrency, _}`, …) is the `beam_bifs` parser's `badarg`, and `ets:info(T, holder)` is `badarg` in the oracle so none is emitted.
- **Scalar `lookup_element` is a deliberate typed refusal for the multi kinds.** `lookup_element`/`lookup_element_or` raise `unsupported_ets` for `bag`/`duplicate_bag` — a scalar `Eterm` cannot express the oracle's *list*; the faithful forms are `lookup_element_all`/`lookup_element_all_or`.
  The BIFs branch on `kind().is_multi()`, so the refusal is unreachable through the BIF surface.
- **`tab2file`/`file2tab`/`tabfile_info` are BLOCKED, not merely out of scope.** The table-file format is `{Header, Objects, Footer}` with every field an ETF term, so it needs `beam_codec` plus file I/O (`beam_io` is wave 5 — an upward edge, forbidden for a wave-1 crate).
  `tab2list` + `first`/`next` remain the enumeration path (`ETS_SURFACE.md` §5).
- **Operations outside this crate's scope** (all in OTP 29): `update_element/4`, the `foldl`/`foldr` **BIFs** (the view layer has `TableRead::{foldl,foldr}`, but the BIFs need a `FunCaller`), `whereis/1`, `table/1,2`, `tab2file*`/`file2tab*` — `ETS_SURFACE.md` §1–§5 names the owner and blocker for each.
  (`ets:clear/1` and `ets:size/1` do not exist in OTP 29, so their absence is correct.)
  The limited forms are implemented; their opaque continuation terms and the bare-vs-`{Results,'$end_of_table'}` shaping are `beam_bifs`'s.
- **`match_spec_run` accepts a RAW spec, unlike the oracle**, which requires `ets:match_spec_compile/1` first (a raw clause list is `badarg`); this crate has no compiled-spec *term*, so `run.rs` compiles the raw spec and the bridge maps `match_spec_compile/1` onto the compiled `MatchSpec`.
- **Named-table lookup-by-name is the runtime's job.** `Table` exposes `name()`/`is_named()` only; the name→table registry is `beam_bifs::ets::TableRegistry` (this crate holds no global state).
- **A hash-kind continuation is an ordinal into iteration order; an `ordered_set`'s is a key anchor.** The `ordered_set` form (`Cursor::Anchor`, an **exclusive** `Table::next`/`prev` bound) is oracle-verified across a mutation *including the anchor's own deletion*.
  The hash kinds keep `Position` (ERTS's cursor there is a bucket/index), so a mutation at or behind the ordinal can skip or repeat an object across a resume straddling it — never a typed error, and never a wrong result for a mutation-free walk.
- **Deep-copy covers every kind the oracle accepts in `ets:insert/2`** — immediates, cons cells, tuples, flat **and hash** maps, funs (all three kinds), boxed leaves (float, bignum, internal ref, on-heap bitstring), external pid/port/reference, and sub-bitstrings / off-heap refc binaries (re-materialised as self-contained on-heap bitstrings).
  The residual typed gaps (`BadArg`, never a wrong result) are a bare `BinRef` root, a native record (`RECORD_SUBTAG`), the reserved boxed subtags, a bare header word, and a malformed `ErlSubBits`.
- **`ets:take/2` retains the checked-out object's arena until the table drops.** `Table::take` copies the removed object out and `relinquish`es the copy: `store.words()` drops by its word count (a take that *drains* the table releases the index too), but the words stay **valid** and never return to the free lists — so take churn grows `blocks` (bounded by the total taken volume) until `store.reset()`/`ets:delete_all_objects/1` or a table drop.
- **The match-spec compiler's typed divergences.** Head-**map** patterns, map **guards** and map **bodies** are oracle parity (the `maps` battery group).
  Typed divergences (never a wrong result): `self/0` resolves through the thread-local `SelfHook` seam, default typed `badarg` (no hook ⇒ a `self/0` guard skips its clause, a `self/0` body is `badarg`); integer arithmetic leaving the 60-bit small range through `DefaultArithHook` is `unsupported_ets`, while `beam_bifs` installs an arbitrary-precision hook around every match-spec-evaluating BIF; a **body** runtime failure yields the atom `EXIT` from the caller's `AtomTable` (a host running a raising body must pre-intern `EXIT`; an uninterned one ⇒ `badarg`).
- **Two match-spec corners live in `src/select/CONTEXT.md`.** `erlang:match_spec_test/3`'s `trace` dialect is left-open (typed `unsupported` — it needs a trace-**runner** subsystem, not a lint-only subset; the `table` dialect *is* implemented), and `select_replace/2`'s static key check has two approximated exotic shapes (a call-shaped raw tuple inside a `{const, X}` subterm compared against a literal head tuple, and the double-tuple unwrap inside a const subterm) — reachable only from hand-written specs, never a wrong result for any spec the battery or the oracle probes.
- **`cmp_arith`'s big-integer↔float key comparison is a best-effort `f64` approximation** (a `beam_term` gap): `number_as_f64` rounds an out-of-range bignum to `±inf`; every representable integer/float pair is exact.
- **The B+-tree's `>= MIN_OCCUPANCY` floor below the root is held up by the delete unwind alone, and the docs read more optimistically than the code.** `merge` drops the parent's separator and child before the parent's own under-full check runs and asserts nothing about the result, and `collapse_root` only collapses a *root* branch that dropped to a single child, so nothing in the production code forbids an internal node from holding a **single** child — a branch is guaranteed only to be non-childless (`node.rs`'s "a branch is never childless", `tree.rs`'s module docs), which a one-child branch satisfies.
  Only the caller-level rebalance restores the floor (a sibling borrow or a merge one level up), and only the test-only `Tree::check_invariants` / `src/ordered_interior/tests.rs::assert_occupancy_floor` assert it, so the shape is reachable only through merge-cascade churn: legal for the invariant set the code states, never observably wrong for a lookup, a walk or `size`, and outside what the pinned occupancy probes ever see.

## Handoffs (to other nodes)
Adopted by the `beam_bifs` bridge (verified): the owner-gated `{protection, Access}` setter (`Table::setopts`/`SetOpt::Access`), the read-gated `ets:slot/2` primitive (`resolve::Gate::Read` — correct for the measured `protected` non-owner ⇒ bucket list / `private` non-owner ⇒ `badarg`), the C-faithful hash-kind `last`/`prev` (no BIF-side kind branch), and match-spec head maps.
The `auto`/`{debug_hash_fixed_number_of_locks, N}` `write_concurrency` rendering stays host-side (`TableInfo::write_concurrency` is a `bool`).
- **`beam_bifs` (open):** install `select::expr::set_self_hook` — no crate does, so `self/0` in a spec silently under-matches (a guard skips its clause, a body yields `EXIT`) where the oracle returns the pid; `BifContext` already carries `self_pid` (`src/select/CONTEXT.md`).
- **`beam_bifs` (open):** reject `[]` in `ets:match_spec_compile/1` (the oracle `badarg`s; the select family legitimately accepts `[]`).
- **`beam_bifs` (adopted, verified) — `ets:give_away/3` gift delivery:** `ets/adopt.rs::give_away_3` → `TableAccess::give_away_with_gift(Some(caller), gift)`, then builds `{'ETS-TRANSFER', Tid, FromPid, GiftData}` on the caller's heap (`Tid` = the `named_table` name atom, else the handle reference; `FromPid` = `ctx.self_pid()`) and sends it through the `SendHook`; with **no** `SendHook` it is the typed `unsupported_bif`, checked *before* the table is taken.
  Its gate is `Gate::Write` plus the owner check inside `give_away_with_gift` — which is why `give_away` never consults `protection` while `setopts` stays owner-gated even on a `public` table.
- **`beam_bifs` (host-owned, verified) — whole-table `ets:delete/1`:** `ets.rs::delete_1` takes the table with `take_table_gated(ctx, handle, resolve::Gate::Write)`, so a non-owner gets `badarg` on `protected`/`private` and may delete a `public` table; `beam_ets` has no whole-table-delete primitive — the runtime drops it via `TableRegistry::remove`/`forget_profile` (+ `remove_name` for a `named_table`).
- **`beam_bifs` (verified) — `ets:setopts/2`'s narrow surface is correct:** `ets/adopt.rs::setopts_2` accepts only `{protection, public|protected|private}` and the `{heir, none|Pid|Pid,Data}` forms (everything else is `badarg`), takes the table with `Gate::Write`, then applies `Table::setopts(Some(owner), SetOpt::Access(_))`, whose owner check denies a `public`-table non-owner — so this crate's narrow `SetOpt` surface is the right one.
- **`beam_bifs` (host rule, verified) — a runtime protection change must NOT re-normalise the fine-locking flags.** Measured: `public` + wc/rc true → `setopts(T,{protection,private})` → `{private, wc true, rc true, dc false}`; a table created `private` keeps `{false,false,false}` after flipping to `public`.
  The normalisation is a creation-time property of `TableOptions`, which is why `Table::set_access` never touches it.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, interior selectors, `Access`, `Concurrency`, `WriteConcurrency`, `TableOptions`, `can_read`/`can_write` (+ `kind/tests.rs`)
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store/mod.rs` → `TermStore` — the table-owned arena / copy boundary (+ `store/tests.rs`)
- `src/table/mod.rs` → `Table`/`TableId`/`OwnerId`, constructors, metadata/ownership accessors, write ops (incl. `release_empty_interior`), access checks, `select_all` (+ `table/tests.rs`); `src/table/read.rs` → the read/navigation `impl Table` (incl. `is_named_table`, `slot`, `tab2list_rev`, the hash-kind `last`/`prev` aliasing); `src/table/rename.rs` → `Table::rename` + `RenameOutcome`
- `src/table/shared.rs` → `SharedTable`; `src/table/view/{mod,read,write,term,iter}.rs` → the guard-scoped `TableRead`/`TableWrite`/`TableTerm`/`HeirView` views (the aliasing defence) + view-borrowed traversal (+ `view/tests.rs`, `view/CONTEXT.md`)
- `src/table/interior.rs` → the private `pub(crate) trait Interior` + `build_interior`; `src/table/access.rs` → `TableAccess`; `src/table/lifecycle.rs` → `OwnerDeath` + `Table::owner_died`; `src/table/element.rs` → the table-level `update_counter`/`update_element`; `src/table/update.rs` → the counter/element operand machinery (shared with the interiors)
- `src/table/ownership.rs` → `Heir`/`SetOpt`/`Transfer`, owner-gated `setopts`, `install_heir`/`heir_data`/`copy_heir`/`replace_heir`, `give_away`/`give_away_with_gift` + the pid-gap docs
- `src/champ_interior.rs` → `ChampInterior` — the **default `set`** interior
- `src/hash_interior/` → the chained hash interior for `bag`/`duplicate_bag` (and opt-in `set`): `mod.rs` (**587** — the type, `with_keypos` key re-anchoring, the `Interior` impl and its `slot` bucket override/`bucket_count()`, inherent `take`/counter/element ops), `capacity.rs` (**263** — the bucket growth/release policy: `rehash_into`/`maybe_grow`/`maybe_shrink`/`trim_objs` + the two shrink/churn unit tests), `tests.rs` (**571** — the rest of its unit tests)
- `src/interior_parity.rs` → cross-interior equivalence + default selection; `src/interior_bench.rs` → interior microbenchmarks (both `#[cfg(test)]`)
- `src/ordered_interior/{mod,sorted,node,tree,iter}.rs` → the B+-tree `ordered_set` interior (with the built-in borrow-or-merge underflow rebalance on the delete unwind, so every non-root node stays `>= MIN_OCCUPANCY` after every `delete`), the sorted-`Vec` alternative, the node arena, the tree ops, the iterator, and the occupancy-floor probes `assert_occupancy_floor` over a `10 000`→1 bulk teardown and a scattered teardown + 10-round churn (driven by the test-only `Tree::{occupancy_profile,leaf_count,arena_stats}`) (+ `tests.rs`; detail in its `CONTEXT.md`)
- `src/select/{mod,pattern,expr,exec,run,continuation}.rs` → the match-spec compiler, executor, runner and continuations (+ its `CONTEXT.md`)
- `src/info.rs` → `TableInfo` + `entries` (the 15-field `ets:info` pairs); `src/error.rs` → the `badarg`/`unsupported` constructors
- `tests/common/mod.rs` → shared integration-test helpers; `tests/common/oracle.rs` → the live-`erl` plumbing; `tests/ets_*.rs`, `tests/select.rs`, `tests/match_spec_run.rs` → the public-API integration suites (what each pins is in Test Strategy)
- `tests/differential.rs` + `tests/differential/{battery/*,erl_program.rs}` → the differential evidence suite vs the system `erl` (191 cases, 15 battery groups); `tests/ets_ordered_oracle.rs` → the randomised mixed-type `ordered_set` differential trace (`#[ignore]`d; run with `--ignored`)

## See Also
- `ETS_SURFACE.md` → the `ets:*` coverage census: every operation mapped to this crate / `beam_bifs` / missing, the oracle-probed semantic facts, and the runtime-level gaps with the exact API each needs.
- `PERF.md` → the measured memory/perf figures (§1 the `ets_copy` peak-RSS attribution, §2 per-object reclamation, §3 the `ordered_set` interior comparison) — both sides of every ratio + the run's `/proc/loadavg`.
- Child nodes: `src/table/CONTEXT.md`, `src/table/view/CONTEXT.md`, `src/store/CONTEXT.md`, `src/select/CONTEXT.md`, `src/ordered_interior/CONTEXT.md`, `tests/CONTEXT.md`.
- `../CONTEXT.md` → crate-layer contracts, the `Table` seam, dependency waves, milestone scope; `../../CONTEXT.md` → workspace contracts (§1 A.1/A.8, §2 frozen term layout, §3 error strategy, §7).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`hashing` utilities; `../beam_alloc/CONTEXT.md` → the `AllocRef`/`AllocType::Ets` seam `TermStore` uses.
- `../../erts/emulator/beam/erl_db.c` / `erl_db_hash.c` / `erl_db_tree.c` / `erl_db_util.c` / `erl_db*.h` → the API, interiors, key/match handling and flags being mirrored (read-only); `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §B/§D → data-structure and ETS copy-trimming discussion.
