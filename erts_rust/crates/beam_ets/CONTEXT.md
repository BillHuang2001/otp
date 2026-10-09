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
  `set_heir(Option<Heir>)`, `set_decentralized_counters(Option<bool>)`; getters `kind()/keypos()/
  access()/is_named()/name() -> Option<Atom>/is_compressed()/write_concurrency()/read_concurrency()/
  heir() -> Option<Heir>/decentralized_counters_option() -> Option<bool>/decentralized_counters() ->
  bool/concurrency() -> Concurrency`; `impl Default`.
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
- Deep-copy **supported subset**: immediates, cons/list cells, tuples, **flat maps**, pointer-free
  boxed leaves (float, bignum, internal ref, on-heap bitstring). **Unsupported** (⇒ `BadArg`): funs,
  hash-maps, records, external pid/port/ref, off-heap bin refs/sub-bitstrings. Every table read can
  only contain what an insert could store, so the copier is complete for table contents.

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
  (1-based), `access`, `owner()/set_owner()`, `heir() -> Option<Heir>` / `set_heir()` / `heir_data()`,
  `give_away(caller, new_owner)`, `setopts(caller, SetOpt)`, `owner_died(dead, is_alive) ->
  OwnerDeath`, `info() -> TableInfo`, `rename(name, is_taken) -> Result<RenameOutcome>`.
- **Writes** (object = the whole tuple; key from `keypos`): `insert` (upsert), `insert_new -> bool`,
  `insert_list(&[object])` (**ATOMIC** — validates and deep-copies the whole list first), `delete(key)`,
  `delete_object(object)`, `delete_all_objects()`.
- **Reads:** `lookup(key) -> Vec<Eterm>` (0/1 for set/ordered_set), `member`, `lookup_element(key, pos)`
  / `lookup_element_or(key, pos, default)` (scalar; `unsupported` raise for the multi kinds),
  `lookup_element_all(key, pos) -> Vec<Eterm>` / `lookup_element_all_or(..) -> Option<Vec<Eterm>>` (the
  multi kinds' list result), `memory()`, `size()` (objects, not keys, for bag/dup_bag), `is_empty()`,
  `tab2list()` (ordered for ordered_set).
- **Element updates:** `update_counter(key, op)`, `update_counter_with(key, op, default)`,
  `update_element(key, &[(pos, term)]) -> bool`.
- **Navigation:** `first()`, `last()`, `next(key)`, `prev(key)`, `take(key) -> Vec<Eterm>` (copies out +
  reclaims the stored originals).
- **Access control:** `check_read(requester)` / `check_write(requester)` / `as_requester(requester) ->
  TableAccess<'_>`.
- **Select family** (`select/exec.rs`; also the `select_all(pred)` predicate primitive): `select(spec)`,
  `select_delete(spec) -> usize`, `select_count(spec) -> usize`, `match_object(pattern)`, `match_(pattern)`,
  `match_delete(pattern) -> usize`, `select_replace(spec) -> usize`.
- **Limit + continuation forms** (`continuation.rs`; `Option::None` ⇒ `'$end_of_table'`): each
  `-> (Vec<Eterm>, Option<MatchContinuation>)`: `select_limit`/`select_continue`/`match_limit`/
  `match_continue`/`match_object_limit`/`match_object_continue`.
- `Table::set_fixation(bool)` / `is_fixed()` / `deferred_objects()` — `ets:safe_fixtable/2`.
- `pub fn match_spec_run(list, spec, atoms, heap) -> Result<Vec<Eterm>>` (`run.rs`) — the
  table-independent `ets:match_spec_run/2` runner.
- `pub(crate) trait Interior` — the internal dispatch seam (`insert`/`insert_new`/`delete`/
  `delete_object`/`lookup`/`member`/`lookup_element`/`lookup_elements`/`size`/`iter_objects`/`keys`/
  `take`/`first_key`/`last_key`/`next_key`/`prev_key`/`memory`/`update_counter`/`update_element`).
  `insert` returns `Result<bool>` — **whether the object was retained** — so `Table::insert` reclaims
  the deep copy it made when the interior rejects it (`bag`'s duplicate is the only `false`). Generic
  defaults rebuild the object on the caller's heap; the hash interior overrides the element updates
  with destructive in-place writes. Private — not part of the cross-crate API.

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
    `list_to_terms()`, `size()`, `eq_term(Eterm, atoms) -> bool`, `cmp_term(Eterm, atoms) -> Ordering`,
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
    `set_fixation`, `set_owner`, `set_heir`, `setopts`, `give_away`), `heir()`, `take ->
    Vec<TableTerm<'_>>`, and `as_requester`.
  - The views are `!Send` by design (they carry a scratch `Heap`): neither a view nor its aliasing
    terms can move to another thread.

### `table/ownership.rs` — `ets:give_away/3` + `{heir, …}` + the heir data
- `pub struct Heir { pub owner: OwnerId, pub data: Option<Eterm> }` — `Some(term)` = `{heir, Pid, Data}`
  (arity 3), `None` = `{heir, Pid}` (arity 2, the C `THE_NON_VALUE`: the heir inherits **silently**,
  with no `{'ETS-TRANSFER'}` message). `pub enum SetOpt { Heir(Heir), ClearHeir }` — the parsed
  `ets:setopts/2` option (the subset modelled).
- `Table::install_heir(owner, data: Option<Eterm>) -> Result<()>` — **copies** the gift data into
  table-owned storage (immediates by value; a non-copyable term is the typed `badarg` and the heir is
  left untouched). `Table::heir_data() -> Option<Eterm>` reads the table-owned copy back.
  `Table::copy_heir(store, heir)` is the shared `pub(super)` copy seam used both by `Table::build`
  (**a creation-time `{heir, …}` is deep-copied at build**, not stored verbatim) and by `install_heir`.
  `Table::replace_heir(heir)` is the shared `pub(super)` reclaim seam every overwrite site
  (`set_heir`, `setopts`, `install_heir`, `build`) funnels through, so the outgoing heir's copy is
  freed exactly once (the C `free_heir_data`). The non-copying primitives `set_heir`/`setopts` stay
  owner-level and documented as such.
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
  `select_replace`; read-checked `lookup`/`member`/`lookup_element(+_or/_all/_all_or)`/`select`/
  `select_count`/`match_object`/`match_`/`first`/`last`/`next`/`prev`/`tab2list`/`size`/`is_empty`/
  `set_fixation` (the oracle gates `safe_fixtable/2` as a READ) and the `/3`+`/1` limited forms.
  `ets:info/1,2` is deliberately **not** gated. `give_away(new_owner)` and `setopts(opt)` are
  requester-implicit (`give_away` has no protection check — ownership is not a read/write op; `setopts`
  is owner-gated, like the oracle).

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
  `first_key`/`last_key`/`next_key`/`prev_key` use the trait defaults.
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
  implements `keys()` but not `first_key`/`last_key`/`next_key`/`prev_key` (trait defaults).

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
  tuple/list/binary/bitstring/map/pid/port/reference/function/boolean`; guard BIFs `element/2`, `abs/1`,
  `size/1`, `hd/1`, `tl/1`, `length/1`, `self/0`. **Arithmetic** (`+ - * / div rem band bor bxor bsl
  bsr`, unary `+`/`-`) routes through the `pub trait ArithHook` seam (default methods) with
  `DefaultArithHook` installed via a `thread_local` (`set_arith_hook`) — no crate edge to `beam_bifs`;
  the default does checked i64 arithmetic (overflow/bignum/float/`/` ⇒ typed `unsupported`, `div`/`rem`
  by zero ⇒ `badarg`, a negative shift count reverses direction, `bsr` saturates — the C `erts_shift`).
  Bodies: sequences (value = last expression), tuple/list construction, literals, `'$N'`, `'$_'`, `'$$'`.
  Guard truth is an internal `Expr::Bool` (independent of whether `true`/`false` are interned). Anything
  outside the recognised set ⇒ `compile`/`expr` `badarg`.
- `exec.rs` — `impl Table`'s select family + `pub enum BodyKind` (`WholeObject`/`AllBindings`/
  `LiteralTrue`) that reuses the compiler for `ets:match*`. `select_replace` is value-validated (the
  clause value must be a tuple of arity ≥ keypos whose keypos element equals the matched key, else
  whole-call `badarg`), supported on set/ordered_set/duplicate_bag (`bag` ⇒ `badarg`). A BODY runtime
  failure is replaced by the atom `EXIT` and evaluation continues (the C `FAIL_TERM`); a GUARD failure
  skips the clause.
- `run.rs` — `match_spec_run(list, spec, atoms, heap)`.
- `continuation.rs` — `pub struct MatchContinuation` (opaque: table id + compiled spec + limit +
  position) and the limited forms, each `-> (Vec<Eterm>, Option<MatchContinuation>)`.

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
- **Files ~1000 lines**, one responsibility per file.
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
- **`insert_list` is atomic; `select_replace/2` is a two-phase value-validated rewrite** (any invalid
  value aborts with `badarg` leaving the table unchanged; the C check is static — see Known Issues).
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

## Test Strategy
- Per-module `#[cfg(test)]` tests in every `src/` module (`store/tests.rs`, `table/view/tests.rs` hold
  the big ones; `interior_parity.rs`/`interior_bench.rs` are `#[cfg(test)]` siblings). Coverage:
  kind/options, key validation, store copy/free/reuse/relinquish, table insert/take accounting,
  hash-interior ops, CHAMP ops (round-trip, 2000-key churn, collision node, canonicalization,
  memory monotonicity), B+-tree invariants (incl. 10 000-key churn), the sorted-`Vec` interior, the
  mixed-class order test, select compile/execute, info, owner-death, the heir forms/reclaim, and the
  guard-scoped views (incl. the aliasing defence and the `compile_fail` doctests).
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
  update,replace,continuation,ownership,fixtable,shared,options,rename}.rs`, `tests/select.rs`,
  `tests/match_spec_run.rs`, sharing helpers from `tests/common/`, and the live-`erl` plumbing in
  `tests/common/oracle.rs`.
- **Differential evidence suite** (the compatibility gate): `tests/differential.rs` runs **135 cases**
  (group files `tests/differential/battery/{kind,keypos,navigation,counter,lookup,matchspec,replace,
  continuation,info,large,access,reclamation,multikind}.rs`, the embedded oracle program in
  `erl_program.rs`) identically under the system `erl` (OTP 29.1.1 / erts-17.1) and under `beam_ets`,
  comparing canonical renderings line-by-line (`mismatch = 0`, `KNOWN_DIVERGENCES` empty). Without
  `erl` the live-oracle tests skip loudly; `BEAM_ETS_REQUIRE_ORACLE=1` turns a skip into a failure.
- **Gates (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets` (407 passed /
  0 failed) and `… --workspace`. Forced-oracle:
  `BEAM_ETS_REQUIRE_ORACLE=1 erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
  beam_ets --test differential`.

## Known Issues (honest gaps — not full parity)
- **The raw `Table`/`TableAccess` read family still returns bare `Eterm`s that alias table storage** —
  the same hazard class the shared handle now closes at compile time. A caller can hold such a term past
  its borrow (or past `take`, or past the table itself) and then observe words reclaimed and reused by
  a later same-size insert. Only `SharedTable` is escape-proof; the fix for the raw API is a cross-crate
  decision (hosts consume it) — `ETS_SURFACE.md` §7.6 names every method that should move behind the
  tagged-term discipline or a scoped `Table::view(&self)` handle.
- **`Concurrency` is advisory.** `read_concurrency`/`write_concurrency` are parsed and reported but
  there is no fine-grained locking; the only synchronisation is `SharedTable`'s coarse `Arc<RwLock>`.
  `{write_concurrency, auto}` and `{debug_hash_fixed_number_of_locks, N}` are not representable (the
  flag is a `bool`).
- **The owner-death transition has no caller.** `Table::owner_died` models it, but nothing in the
  runtime invokes it: `beam_bifs`'s `ets:new/2` never records an owner, its `TableRegistry` has no
  owner index, and `beam_process` has no process-death hook at all. `ETS_SURFACE.md` §7 names the exact
  API each of those needs.
- **`ets:info/1` `owner`/`heir` are `undefined` and `id` is a small integer.** `owner` (a pid in the
  oracle) has no term representation at this stage; `heir` is `none` when unset and `undefined` when
  installed (its structured `TableInfo::heir` still carries the pid); `id` renders as a small integer
  rather than a ref.
- **Ownership modelling has no pid/liveness concept.** `give_away/3` treats `None` as the oracle's
  non-pid/dead-pid target (both `badarg`); an unknown table id is the registry/BIF layer's concern.
  `Table::owner_died` decides the transition and returns `OwnerDeath`; the delivery of the transfer
  message and the dropping of the table stay the runtime's job.
- **`ets:setopts/2` is modelled only for `{heir, …}`.** Other options (`safe_fixtable`,
  `{write_concurrency, _}`, …) are out of scope; `ets:info(T, holder)` is `badarg` in the oracle and
  none is emitted. The BIF itself is still a seam-backed `unsupported_bif` in `beam_bifs`.
- **Scalar `lookup_element` is unsupported for the multi kinds.** `Table::lookup_element`/
  `lookup_element_or` raise `unsupported_ets` for `bag`/`duplicate_bag` (the oracle returns a *list*);
  the faithful forms are `lookup_element_all`/`lookup_element_all_or`. **Handoff:** the `beam_bifs`
  bridge must route the multi kinds through those.
- **`tab2file`/`file2tab`/`tabfile_info` are BLOCKED, not merely out of scope.** The table-file format
  is `{Header, Objects, Footer}` with every field an ETF term, so it needs `beam_codec` plus file I/O
  (`beam_io` is wave 5 — an upward edge, forbidden for a wave-1 crate). `tab2list` + `first`/`next`
  remain the enumeration path. Full reasoning in `ETS_SURFACE.md` §5.
- **Operations outside this crate's scope** (all present in OTP 29): `update_element/4`,
  `select_reverse/*`, `foldl`/`foldr`, `whereis/1`, `table/1,2`, `slot/2`, `tab2file*`/`file2tab*` —
  `ETS_SURFACE.md` §1–§5 names the owner and the blocker for every one. (`ets:clear/1` and `ets:size/1`
  do not exist in OTP 29, so their absence is correct.) The `/3` limited forms are implemented; their
  opaque continuation terms and the bare-vs-`{Results,'$end_of_table'}` shaping are `beam_bifs`'s.
- **`match_spec_run` accepts a RAW spec, unlike the oracle.** The oracle requires
  `ets:match_spec_compile/1` first (a raw clause list is `badarg`); this crate has no compiled-spec
  *term*, so `run.rs` compiles the raw spec and the bridge maps `match_spec_compile/1` onto the compiled
  `MatchSpec`.
- **Named-table lookup-by-name is the runtime's job.** `Table` exposes `name()`/`is_named()` only; the
  name→table registry is the sibling `beam_bifs::ets::TableRegistry` (this crate holds no global state).
- **Deep-copy is a subset.** Funs, hash-maps, records, external pid/port/ref and off-heap bin
  refs/sub-bitstrings cannot be copied into table storage (`insert` ⇒ `BadArg`).
- **The match-spec compiler covers the common guard set**, not the full Erlang guard language
  (`map_get`, `float`/`trunc`/`round`, `list_to_tuple`, comprehensions in bodies, …) — anything outside
  it is `badarg`. Match-spec arithmetic via the default `ArithHook` is small-int only; a real runtime
  installs a full hook via `set_arith_hook`.
- **`select_replace/2`'s key check is value-based, not the C's static syntactic check.** The oracle
  (`db_match_keeps_key`) rejects a body that is not syntactically `'$_'`/`{{…}}`/`{const,{…}}`/
  `{element,KeyPos,'$_'}` even when nothing matches; this crate validates the *value*, so an exotic
  multi-expression body diverges (pinned by the `recorded_divergence_*` tests). Wherever the oracle
  accepts, the results agree.
- **`cmp_arith`'s big-integer↔float key comparison is a best-effort `f64` approximation** (a
  pre-existing `beam_term` gap). ERTS compares an integer key against a float key numerically;
  `number_as_f64` rounds an out-of-range bignum to `±inf`. Every representable integer/float pair
  (including the small-int/float keys the tests pin) is exact.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, `SetInterior`/`OrderedInterior`, `Access`, `Concurrency`, `TableOptions` (incl. heir + `decentralized_counters`), `can_read`/`can_write`
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store/mod.rs` → `TermStore` (table-owned arena + copy/free/relinquish/reuse) — the copy boundary
- `src/store/tests.rs` → the `TermStore` unit-test module
- `src/table/mod.rs` → `Table`/`TableId`/`OwnerId`, constructors, metadata/ownership accessors, write ops, access checks, `select_all`, module wiring
- `src/table/read.rs` → the read/navigation `impl Table`
- `src/table/rename.rs` → `Table::rename` + `RenameOutcome` (`ets:rename/2` semantics)
- `src/table/shared.rs` → `SharedTable` (the locked handle) + its unit tests
- `src/table/view/mod.rs`, `view/read.rs`, `view/write.rs`, `view/term.rs` → the guard-scoped `TableRead`/`TableWrite`/`TableTerm` views (the aliasing defence)
- `src/table/view/tests.rs` → the view-layer unit tests (incl. the aliasing evidence)
- `src/table/interior.rs` → the private `pub(crate) trait Interior` + `build_interior`
- `src/table/access.rs` → `TableAccess` (access-enforcing view)
- `src/table/ownership.rs` → `Heir`/`SetOpt`, `install_heir`/`heir_data`/`copy_heir`/`replace_heir`, the `give_away`/`setopts` semantics + the pid-gap docs
- `src/table/lifecycle.rs` → `OwnerDeath` + `Table::owner_died`
- `src/table/element.rs` → the table-level `update_counter`/`update_element` operations
- `src/table/update.rs` → the counter/element operand machinery (shared with the interiors)
- `src/champ_interior.rs` → `ChampInterior` (CHAMP index) — the **default `set`** interior
- `src/hash_interior.rs` → `HashInterior` (chained index) for `bag`/`duplicate_bag` (and opt-in `set`)
- `src/interior_parity.rs` → cross-interior equivalence + default-selection tests (`#[cfg(test)]`)
- `src/interior_bench.rs` → the interior microbenchmarks (`#[cfg(test)]`)
- `src/ordered_interior/{mod,sorted,node,tree,iter}.rs` → the B+-tree `ordered_set` interior, the sorted-`Vec` alternative, the node arena, the tree ops, the iterator
- `src/select/{mod,pattern,expr,exec,run,continuation}.rs` → the match-spec compiler, executor, runner and continuations
- `src/info.rs` → `TableInfo` + `entries` (the 15-field `ets:info` pairs)
- `src/error.rs` → `badarg`/`unsupported` failure constructors
- `tests/common/mod.rs` → shared integration-test helpers; `tests/common/oracle.rs` → the live-`erl` plumbing
- `tests/ets_*.rs` → public-API integration tests by concern (basic/order/select/bag/info/nav/access/update/replace/continuation/ownership/fixtable/shared/options/rename), plus `tests/select.rs` and `tests/match_spec_run.rs`
- `tests/differential.rs` + `tests/differential/{battery/*,erl_program.rs}` → the differential evidence suite vs the system `erl` (135 cases)
- `ETS_SURFACE.md` → the `ets:*` coverage census + the oracle-probed semantic facts + the runtime-level gaps and the exact API each needs
- `../../erts/emulator/beam/erl_db.c` / `erl_db_hash.c` / `erl_db_tree.c` / `erl_db_util.c` / `erl_db*.h` → the API, interiors, key/match handling and flags being mirrored (read-only)

## See Also
- `ETS_SURFACE.md` → coverage census: every `ets:*` operation mapped to this crate / `beam_bifs` / missing, the oracle-probed facts, and the runtime-level gaps with the exact API each needs.
- `../CONTEXT.md` → crate-layer contracts, the `Table` seam, dependency waves, milestone scope.
- `../../CONTEXT.md` → workspace contracts (§1 A.1/A.8, §2 frozen term layout, §3 error strategy, §7).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`hashing` utilities.
- `../beam_alloc/CONTEXT.md` → the `AllocRef`/`AllocType::Ets` allocation seam `TermStore` uses.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §B/§D → data-structure and ETS copy-trimming discussion.
