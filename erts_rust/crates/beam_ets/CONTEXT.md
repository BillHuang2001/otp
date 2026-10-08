# `beam_ets` — ETS Tables (Stage 3 data structures)

## Intent
Owns the **observable semantics of `ets:*`** — Erlang Term Storage — over **flat, table-owned term
storage** (the A.1 "copy boundary"). Implements Part I §A.1 (term/key/value semantics) and §A.8
(`ets:info/1` introspection) and the Stage-3 row of the plan (Part II §H): `set` over a **CHAMP**
(compressed hash-array mapped prefix-tree) open-addressing index, `bag`/`duplicate_bag` over a
**chained-bucket** hash index, and `ordered_set` over a **K-ary B+-tree**.

The **interior data structures are NOT externally pinned** (crate-layer `../CONTEXT.md` §2): bucket
layout, hash-map thresholds, tree fan-out and load factors are implementation choices. Only the
**semantics** are contract — which object wins on a duplicate key, key equality/ordering, `size`, and
what `lookup`/`select`/`tab2list`/`first`/`next` return. Term **ordering** and **hashing** are owned
by `beam_term` (`compare::cmp`/`cmp_arith`/`eq`, `hashing::term_hash`) and MUST NOT be re-implemented here.

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
- `pub enum SetInterior { Chained, #[default] Champ }` (default **`Champ`**) — the crate-internal
  selection of the `set` interior (`as_str() -> "chained"`/`"champ"`); `Table::with_set_interior`
  installs it, `Table::set_interior` reports it. Not a `TableOptions` field (no BIF surface) and not
  consulted by `bag`/`duplicate_bag`/`ordered_set`.
- `pub enum OrderedInterior { #[default] BPlusTree, Sorted }` (default **`BPlusTree`**) — the
  crate-internal selection of the `ordered_set` interior (`as_str() -> "bplus_tree"`/`"sorted"`);
  `Table::with_ordered_interior` installs it, `Table::ordered_interior` reports it. Not a
  `TableOptions` field and not consulted by `set`/`bag`/`duplicate_bag`. `Sorted` selects
  `SortedOrderedInterior` (a sorted `Vec`, `O(log n)` binary-search lookup with `O(n)`
  insert/delete) so the two ordered interiors can be cross-checked and measured; the B+-tree stays
  the default (Design Decisions).
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
    pub fn with_set_interior(id: TableId, opts: TableOptions, interior: SetInterior) -> Result<Table>; // opt-in `set` interior
    pub fn set_interior(&self) -> SetInterior;   // the selected `set` interior (Champ by default)
    pub fn with_ordered_interior(id: TableId, opts: TableOptions, interior: OrderedInterior) -> Result<Table>; // opt-in `ordered_set` interior
    pub fn ordered_interior(&self) -> OrderedInterior; // the selected `ordered_set` interior (BPlusTree by default)

    // metadata
    pub fn id(&self) -> TableId;
    pub fn kind(&self) -> TableKind;
    pub fn name(&self) -> Option<Atom>;      // Some iff a name was recorded (independent of named_table)
    pub fn keypos(&self) -> usize;           // 1-based
    pub fn access(&self) -> Access;
    pub fn owner(&self) -> Option<OwnerId>;
    pub fn set_owner(&mut self, owner: Option<OwnerId>);
    pub fn give_away(&mut self, caller: Option<OwnerId>, new_owner: Option<OwnerId>) -> Result<()>;
    pub fn heir(&self) -> Option<Heir>;
    pub fn set_heir(&mut self, heir: Option<Heir>);
    pub fn setopts(&mut self, caller: Option<OwnerId>, opt: SetOpt) -> Result<()>;
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
    pub fn select_replace(&mut self, spec: &MatchSpec, atoms: &AtomTable, heap: &mut Heap) -> Result<usize>;

    // limit + continuation forms (see continuation.rs; `Option::None` ⇒ '$end_of_table')
    pub fn select_limit(&self, spec: &MatchSpec, limit: usize, atoms: &AtomTable, heap: &mut Heap) -> Result<(Vec<Eterm>, Option<MatchContinuation>)>;
    pub fn select_continue(&self, cont: &MatchContinuation, atoms: &AtomTable, heap: &mut Heap) -> Result<(Vec<Eterm>, Option<MatchContinuation>)>;
    pub fn match_limit(&self, pattern_term: Eterm, limit: usize, atoms: &AtomTable, heap: &mut Heap) -> Result<(Vec<Eterm>, Option<MatchContinuation>)>;
    pub fn match_continue(&self, cont: &MatchContinuation, atoms: &AtomTable, heap: &mut Heap) -> Result<(Vec<Eterm>, Option<MatchContinuation>)>;
    pub fn match_object_limit(&self, pattern_term: Eterm, limit: usize, atoms: &AtomTable, heap: &mut Heap) -> Result<(Vec<Eterm>, Option<MatchContinuation>)>;
    pub fn match_object_continue(&self, cont: &MatchContinuation, atoms: &AtomTable, heap: &mut Heap) -> Result<(Vec<Eterm>, Option<MatchContinuation>)>;
}
```
- `pub fn match_spec_run(list: Eterm, spec: Eterm, atoms: &AtomTable, heap: &mut Heap) -> Result<Vec<Eterm>>` (from `run.rs`) — the table-independent `ets:match_spec_run/2` runner.
- `pub(crate) trait Interior` — the internal dispatch seam implemented by the two interiors
  (`insert`/`insert_new`/`delete`/`delete_object`/`lookup`/`member`/`lookup_element`/`lookup_elements`/`size`/
  `iter_objects`/`keys`/`take`/`first_key`/`last_key`/`next_key`/`prev_key`/`memory`/
  `update_counter`/`update_element`). `Table` stores `Box<dyn Interior>`. `insert` returns
  `Result<bool>` — **whether the object was retained** — so `Table::insert` reclaims the deep copy it
  made when the interior rejects it as a duplicate (`bag` is the only kind that can return `false`;
  `set`/`ordered_set`/`duplicate_bag` always retain). Generic defaults rebuild the
  object on the caller's heap (what the B+-tree needs); the hash interior overrides the element
  updates with **destructive in-place writes**. Private — **not** part of the cross-crate API.

### `table/ownership.rs` — `ets:give_away/3` + the `{heir, ...}` option
- `pub struct Heir { pub owner: OwnerId, pub data: Eterm }` — a table heir (the inheriting
  process + the `{'ETS-TRANSFER', Tab, FromPid, GiftData}` gift data).
- `pub enum SetOpt { Heir(Heir), ClearHeir }` — the parsed `{heir, ...}` option of
  `ets:setopts/2` (the subset modelled).
- The module holds the oracle semantics for `Table::give_away`/`setopts` (see Design Decisions)
  and the **pid-gap** documentation: a `None` new owner stands in for a non-pid/dead pid target,
  an unknown table id is the registry/BIF layer's concern, and a set heir renders as `undefined`
  (no pid term).

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
  The two ownership wrappers are requester-implicit: `give_away(&mut self, new_owner: Option<OwnerId>)
  -> Result<()>` (the requester is the caller; **no** `protection` check — ownership is not a
  read/write op) and `setopts(&mut self, opt: SetOpt) -> Result<()>` (owner-gated, like the oracle).
  The underlying `Table` methods stay owner-level primitives (no access check). The matrix is
  verified against the system `erl` (OTP 29 / erts-17.1).

### `table/update.rs` — `ets:update_counter`/`update_element` operand machinery
- `pub(crate)` helpers shared with the hash interior: `counter_slot`, `add_counters`,
  `replace_elements` (re-exported from `table/mod.rs` so the `crate::table::<fn>` paths hold);
  internals `object_arity`, `read_counter`, `counter_operand`, `counter_operand_error`, `warp_fires`,
  `decode_counter_ops`, `decode_counter_tuple`, `build_list`, `CounterOp`.

### `champ_interior.rs` — CHAMP open-addressing interior (**default `set`**)
- `pub struct ChampInterior` + `Default`/`new()`: a **compressed hash-array mapped prefix-tree**
  (CHAMP). Each level consumes `WIDTH = 5` bits of the 64-bit `term_hash` (`index_bit`) and every node
  keeps its children **compressed** into two bitmaps — `datamap` (inline slots) and `nodemap`
  (sub-nodes) — so `data`/`nodes` hold only the set bits in ascending bit order (`slot_index` finds a
  slot by popcount). A key whose full hash collides with another's lives in a `CollisionNode`
  (popcount-free, key-compared with `eq`), so a pathological hash can never recurse past 64 bits.
  `Interior` impl: `insert` is an upsert (returns `Ok(true)` — `set` always retains), and
  `insert_new`/`delete`/`delete_object`/`lookup`/`member`/`lookup_element`/`size`/`iter_objects`/
  `keys`/`memory` are all overridden. `lookup`/`lookup_element` probe with `slot_index` only (no
  bucket scan). `iter_objects`/`keys` walk depth-first (`collect_into`) — the `set` navigation order
  is unspecified, so `first_key`/`last_key`/`next_key`/`prev_key` and the `ets:update_*` element ops
  use the `Interior` trait defaults. `memory()` counts the root inline plus every node/vector
  (`heap_words`).
- **Canonicalization (delete).** A removal calls `canonicalize` bottom-up: a sub-node holding exactly
  one entry (`single_entry`) is **inlined** into its parent as an inline slot (`bit_at` locates the
  parent's bit), collapsing single-entry chains. Promoting a *multi-entry* sub-node wholesale is
  provably wrong — a sub-node is indexed by the *next* 5 hash bits, so it must not be probed with the
  parent's bits; only single-entry inlining preserves routing. `node_insert` needs no
  canonicalization (an insert only adds entries).
- Selection: [`SetInterior`] (`kind.rs`) — `Champ` (default) or `Chained`; `build_interior(kind,
  set_interior, ordered_interior)` routes `TableKind::Set` accordingly (and `TableKind::OrderedSet`
  through [`OrderedInterior`]). `bag`/`duplicate_bag` stay on `HashInterior`; `ordered_set` is the
  B+-tree (default) or the sorted `Vec`. Constructor: `Table::with_set_interior`;
  `Table::set_interior` reports the selection.

### `hash_interior.rs` — chained hash-index interior (`bag`/`duplicate_bag`; opt-in `set`)
- `pub struct HashInterior`: buckets `Vec<Vec<Entry>>` (`Entry { key, objs: Vec<Eterm> }`), lookup by
  `term_hash(key) % buckets` then `eq`; doubles + rehashes past a load factor. Used by `bag`/
  `duplicate_bag` and by `set` when [`SetInterior::Chained`] is selected. For `set` it keeps ≤ 1 object
  per key; `bag` rejects a duplicate *object* within a key via `eq` (its `Interior::insert` returns
  `false`, so the caller reclaims that rejected copy); `duplicate_bag` keeps a multiset.
  Public API for testing/reuse: `new(kind: TableKind)`, `take(key, atoms) -> Vec<Eterm>`,
  `update_counter(key, pos, incr, atoms) -> Result<Eterm>` (set/ordered_set semantics; in-place),
  `update_element(key, updates, atoms) -> Result<bool>`, `memory(atoms) -> usize`.
  It implements `keys()` (bucket-scan order) but **not** `first_key`/`last_key`/`next_key`/`prev_key`,
  so the trait defaults supply them (see Design Decisions, hash-kind navigation order).

### `ordered_interior/` — the ordered `ordered_set` interiors (B+-tree + sorted `Vec`)
- `mod.rs` — `pub struct OrderedSetInterior` (key-ordered B+-tree, see Design Decisions): `new()`,
  `first`/`last`, `first_key`/`last_key`, `next`/`prev`, `next_from(bound, inclusive)`/
  `prev_from(bound, inclusive)` (the partial-bound range searches) — all via `beam_term::compare::cmp_arith`
  (the **arithmetic** key order ERTS `ordered_set` uses — `erl_db_tree.c` `cmp_key_eq`,
  `CMP = erts_cmp(a, b, 0, 0)` — so `1` and `1.0` are the *same* key). Whole-object identity
  (`delete_object/2`) stays the exact `compare::eq`.
  Plus its `Interior` impl (which overrides `first_key`/`last_key`/`next_key`/`prev_key` and
  `memory`). Interior storage (the node arena) **is** counted by `memory()`.
- `sorted.rs` — `pub struct SortedOrderedInterior`: the alternative `ordered_set` interior
  ([`OrderedInterior::Sorted`]), the **same** observable semantics over one `Vec<Entry>` kept sorted
  by `beam_term::compare::cmp_arith` (`O(log n)` binary-search lookup via the shared `node::search_entries`,
  `O(n)` `Vec::insert`/`Vec::remove`), with the same inherent navigation and a capacity-based
  `memory()`. Crate-internal `iter()` yields the shared `node::Entry` record.
- `node.rs` — the arena node layout + `ORDER`/`MIN_OCCUPANCY` constants and the `NodeStore`
  (`NodeId` slots + free list; nodes never relocate); also the shared `Entry` record,
  `search_entries` (used by both interiors) and `words_of`.
- `tree.rs` — insert/lookup/delete with split + borrow-or-merge rebalance and the separator invariant
  `keys[i] == max(child[i])`.
- `iter.rs` — the leaf-chain ordered iterator.

### `select/` — the match-spec compiler + executor
- `mod.rs` — `pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;`
  `pub struct MatchSpec { pub clauses: Vec<MatchClause> }`,
  `pub struct MatchClause { head, guard, body }`, `pub fn compile(spec: Eterm, atoms: &AtomTable) -> Result<MatchSpec>`;
  type aliases `MatchPattern`/`MatchElement` = `Pattern`, `MatchGuard`/`MatchBody`/`MatchBodyElement` = `Expr`.
- `pattern.rs` — `pub enum Pattern` (nested tuple/list/improper patterns, quoted literals, `'_'`,
  `'$N'` binds with repeat-occurrence equality; every other atom — including `'$_'`/`'$$'`, which are
  special only in a *body* — is an exact `=:=` literal).
- `expr.rs` — `pub enum Expr`, `pub enum CmpOp`, `pub enum TypeTest`, `pub enum ArithOp`: the
  guard/body expression language. Guards: `andalso`/`and`, `orelse`/`or`, `not`, `const`, bare
  truthiness; comparisons `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=` (structural `cmp` for exact,
  `cmp_arith` for `==`-family); type tests `is_atom/integer/float/number/tuple/list/binary/bitstring/
  map/pid/port/reference/function/boolean`; guard BIFs `element/2`, `abs/1`, `size/1`, `hd/1`, `tl/1`,
  `length/1`, `self/0`. **Arithmetic** (`+ - * / div rem band bor bxor bsl bsr`, unary `+`/`-`) is
  routed through the `pub trait ArithHook` seam (default methods) with `pub struct DefaultArithHook`
  installed via a `thread_local` (`pub fn set_arith_hook`) so there is no crate edge to `beam_bifs`;
  the default does checked small-int (i64) arithmetic — overflow/bignum/float/`/` ⇒ a typed
  `unsupported`, `div`/`rem` by zero ⇒ `badarg`, a negative shift count reverses direction and `bsr`
  saturates (the C `erts_shift`). Bodies: sequences (value = last expression), tuple/list
  construction, literals, `'$N'`, `'$_'`, `'$$'`. Guard truth is an internal `Expr::Bool` (independent
  of whether `true`/`false` are interned). Anything outside the recognised set ⇒ `compile`/`expr`
  `badarg` (Known Issues).
- `exec.rs` — the `impl Table` block for the select family (`select`/`select_delete`/`select_count`/
  `match*`/`select_replace`) + `pub enum BodyKind` (`WholeObject`/`AllBindings`/`LiteralTrue`) that
  reuses the compiler for `ets:match*`. `select_replace` is value-validated (the clause value must be
  a tuple of arity ≥ keypos whose keypos element equals the matched key, else whole-call `badarg`),
  supported on set/ordered_set/duplicate_bag (`bag` ⇒ `badarg`). A BODY runtime failure is replaced by
  the atom `EXIT` and evaluation continues (the C `FAIL_TERM`); a GUARD failure skips the clause.
- `run.rs` — `pub fn match_spec_run(list, spec, atoms, heap) -> Result<Vec<Eterm>>`: the
  table-independent `ets:match_spec_run/2` runner (compiles the raw spec; the oracle requires
  `ets:match_spec_compile/1` first — a `beam_bifs` bridge handoff).
- `continuation.rs` — `pub struct MatchContinuation` (opaque: table id + compiled spec + limit +
  position) and the limited forms `select_limit`/`select_continue`/`match_limit`/`match_continue`/
  `match_object_limit`/`match_object_continue`, each returning `(Vec<Eterm>, Option<MatchContinuation>)`
  (`None` ⇒ the oracle's `'$end_of_table'`).

### `info.rs` — introspection (`ets:info`)
- `pub struct TableInfo { id, name: Option<Atom>, kind, keypos, access, size, memory, owner:
  Option<OwnerId>, heir: Option<OwnerId>, named_table: bool, compressed: bool, write_concurrency:
  bool, read_concurrency: bool }`;
  `pub fn entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)>` — the heap-free `{Key, Value}`
  pairs of the oracle's **15** `ets:info/1` fields, in the oracle's order (`id`,
  `decentralized_counters`, `read_concurrency`, `write_concurrency`, `compressed`, `memory`, `owner`,
  `heir`, `name`, `size`, `node`, `named_table`, `type`, `keypos`, `protection`). `heir` is the atom
  `none` when unset and `undefined` when set (a pid gap — see below) and `node` is the runtime-shape
  constant `nonode@nohost`; `decentralized_counters` follows the oracle's CATREE
  rule (`ordered_set && write_concurrency`); `owner` (a pid in the oracle) renders as `undefined` and
  `id` as a small integer (see Known Issues). The *observable* `ets:info/1` proplist is built by the
  sibling `beam_bifs` bridge, not by this method — see the Known Issues handoff.

### `error.rs` — failure constructors
- `pub(crate) fn badarg() -> BeamError` — the `badarg` failures (bad object/`keypos`, absent element,
  malformed spec, …).
- `pub(crate) fn unsupported(what: &str) -> BeamError` — a `BeamError::Raise` (error class) with
  reason atom `unsupported_ets: <what>`, for an operation the crate deliberately does not implement
  (currently the scalar `lookup_element` on a multi kind). No crate-local error enum.

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`. `badarg`-style failures ⇒ `BeamError::BadArg`;
  never `panic!`/`unwrap()` on runtime data; no crate-local error enum. Table ops never panic on
  malformed input — they return `Err`.
- **Never re-implement term order/hash.** Ordering ⇒ `beam_term::compare::cmp` (the exact `=:=` order)
  / `cmp_arith` (the **arithmetic** order ERTS `ordered_set` keys use) / `eq` (whole-object identity);
  hashing ⇒ `beam_term::hashing::term_hash` (workspace Prevent-Duplication rule).
- **Table-owned storage only (A.1 copy boundary).** An inserted object is **deep-copied** into the
  table's `TermStore`; the table never retains a pointer into a caller's/process heap. Objects/keys
  returned from reads alias table storage and stay valid for the table's lifetime.
- **`unsafe` confinement.** `unsafe` is confined to `store/` (raw term reads/writes), the
  tuple-element accessor in `key.rs`, and the small term-reading blocks in `select/` and
  `table/mod.rs` (`select_all`). Every `unsafe fn` carries a `# Safety` section and every `unsafe`
  block a preceding `// SAFETY:` rationale (`beam_common::safety`); `unsafe_op_in_unsafe_fn` stays
  clean. `kind`/`info`/`error`/`champ_interior`/`hash_interior`/`ordered_interior` and most of `table/`
  are safe Rust.
- **Files ~1000 lines**, one responsibility per file.
- **Licensing** Apache-2.0; no new external deps (only `beam_common`/`beam_term`/`beam_alloc`/`libc`).

## Design Decisions
- **`ordered_set` defaults to a K-ary B+-tree** (`ORDER = 32`, `MIN_OCCUPANCY = ORDER/2 = 16`),
  replacing the earlier sorted-`Vec` placeholder as the *only* interior (a fresh sorted-`Vec`
  interior is retained as the selectable comparison index, see the next-next bullet). Nodes live in
  an arena-backed `NodeStore` addressed by `NodeId` (with a free list), so **nodes never relocate**
  and no interior pointer is ever invalidated — matching `erl_db_tree.c`'s shape and giving O(log n)
  insert/lookup/delete and O(log n) ordered iteration via a leaf chain plus the separator invariant
  `keys[i] == max(child[i])`. Rebalancing is borrow-or-merge on underflow. All key comparisons go
  through `beam_term::compare::cmp_arith` (the **arithmetic** key order ERTS `ordered_set` uses —
  `erl_db_tree.c` `cmp_key_eq`, `CMP = erts_cmp(a, b, 0, 0)`), so `1` and `1.0` are the *same* key
  (the last-inserted object wins; `delete_object/2` still compares whole objects with the exact
  `compare::eq`). Recorded as a structural
  change for `../../CHANGE.md` (repo-root owned).
- **`set` defaults to a CHAMP open-addressing interior; `bag`/`duplicate_bag` stay chained.** Adopted
  on measurement, not faith: `src/interior_bench.rs` compares `HashInterior` (chained) against
  `ChampInterior` per operation across 4 key shapes × 4 sizes (1e1/1e3/1e5/1e6), timing the *index*
  only (the `TermStore` copy/match cost is common to both). CHAMP's `champ/old` ratios (CHAMP ÷
  chained) across shapes/sizes: **insert 0.08–1.07×** (2–12× faster from 1e3 up), **iteration
  0.20–1.48×** (2–5× faster from 1e3 up), **memory 0.14–0.75×** (2–7× fewer index bytes/entry —
  e.g. smallint 1e6: 28 vs 194 B), **lookup-hit 0.20–1.50×** (parity once `eq` dominates on
  binary/tuple keys), but **delete 0.69–2.77×** (consistently 1.6–2.8× *slower*: a removal walks down
  and canonicalizes) and, on small-int keys, **lookup-miss up to ~2.1× slower** (proving a miss
  descends a node chain vs one bucket probe). Net win is clear for the insert/iterate/memory profile
  ETS is optimised for, so `SetInterior::default() == Champ`; the chained interior remains selectable
  for `set` via `SetInterior::Chained` (`Table::with_set_interior`) and is still the index for
  `bag`/`duplicate_bag`. The interior shape is unpinned (workspace §2), and the parity tests prove no
  observable `ets:*` behaviour changes. (Recorded as a structural change for `../../CHANGE.md`.)
- **`ordered_set` keeps the B+-tree as the default; the sorted `Vec` stays selectable.** Adopted on
  measurement, not faith: `src/interior_bench.rs`'s `ordered_interior_microbenchmark` compares
  `OrderedSetInterior` (B+-tree) against `SortedOrderedInterior` (sorted `Vec`) per operation across 4
  key shapes × the sizes `10`/`1e3`/`1e5`, driving the keys in a **shuffled** order (mounting them in
  ascending order would flatter the sorted `Vec`'s O(1)-append fast path and its shift-the-tail delete
  worst case). `sorted/tree` ratios (sorted ÷ B+-tree) at `n = 1e5`: **insert 3.9–6.3× slower** and
  **delete 3.1–34.6× slower** — the `O(n)` `Vec::insert`/`Vec::remove` tail shift grows with `n`
  (already 1.5–9.8× / 1.7–4.1× at `n = 1e4`), against **lookup-hit 0.66–1.05×** (parity),
  **lookup-miss 0.32–0.76×**, **ordered-iterate ≈0.1–0.45×** and **memory 0.73×** (27% fewer
  bytes/entry — 21 vs 29 B). The sorted interior's *read* wins are bounded constants, while its
  *mutating* loss is `O(n)` and unbounded, so it can never be the default for a table whose defining
  operations are insert/lookup/delete; the B+-tree's `O(log n)` mutation is why ETS uses a tree. Hence
  `OrderedInterior::default() == BPlusTree`; the sorted `Vec` remains selectable for comparison via
  `OrderedInterior::Sorted` (`Table::with_ordered_interior`). The interior shape is unpinned (workspace
  §2), and `src/interior_parity.rs` proves the two ordered interiors produce identical observable
  traces — including the exact ascending order. (Recorded as a structural change for `../../CHANGE.md`.)
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
  on an **absent** anchor are `badarg`. `ordered_set` navigation is exact (ascending arithmetic key
  order, `compare::cmp_arith`;
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
- **The registered `name` and the `named_table` flag are independent.** `set_name` records the name
  only; `set_named`/the `named_table` option sets the flag — mirroring the oracle
  (`ets:new(myanon, [])` ⇒ `name => myanon` but `named_table => false`).
- **A `private` table reports the concurrency flags as `false`.** The `TableOptions`
  `write_concurrency()`/`read_concurrency()` getters (hence `concurrency()`) normalize to `false`
  when `access == Private`, matching the oracle (`is_fine_locked && !DB_PRIVATE` in `erl_db.c`);
  `compressed` is unaffected. Normalizing in the getters keeps `TableOptions`, `TableInfo` and the
  bridge in agreement and makes the rule order-independent (access may be set before or after).
- **`ets:info/1` mirrors the oracle's 15 fields in the oracle's order.** `TableInfo::entries` emits
  `id, decentralized_counters, read_concurrency, write_concurrency, compressed, memory, owner, heir,
  name, size, node, named_table, type, keypos, protection` — the system `erl`'s exact proplist order
  (the reverse of the C `fields[]` array in `erl_db.c:4406`). `heir`/`node` are the runtime-shape
  constants `none`/`nonode@nohost`; `decentralized_counters` is `ordered_set && write_concurrency` (the
  oracle's CATREE rule — the explicit `{decentralized_counters, Bool}` creation option is unsupported).
  The public `TableInfo` gained a structured `heir: Option<OwnerId>` field (the *pid* only — the gift
  data is never reported), populated by `Table::info`; the rendering-only runtime-shape fields are
  still computed in `entries`.
- **Ownership/heir semantics are a `Table`-level concern, not an access-mode one.** `give_away/3` and
  `setopts/2` do **not** consult `protection`; they are owner-gated (the oracle's
  `EXI_NOT_OWNER`/`EXI_OWNER` `badarg`s). `give_away` follows the C `ets_give_away_3` order (bad-pid →
  not-owner → self-gift, all `badarg`, owner unchanged on failure); `setopts` requires the caller to be
  the owner. The heir is stored as a full `Heir` (pid + gift data) for the runtime's `{'ETS-TRANSFER',…}`
  message, but `ets:info` renders only the pid — which this crate cannot represent (the documented gap).
- **`insert_list` is atomic.** It validates *and deep-copies* every object before the first index
  mutation, freeing the copies on failure — so a `badarg` anywhere in the list leaves the table
  unchanged, as the oracle does.
- **`select_replace/2` is a two-phase value-validated rewrite.** Phase one evaluates and validates
  every matching object's clause value (a tuple of arity ≥ keypos whose keypos element `eq`s the
  matched object's key) *and* deep-copies it into table storage; phase two swaps the objects in. Any
  invalid value aborts the whole call with `badarg` leaving the table unchanged. Returns the count of
  replaced objects. `bag` ⇒ `badarg`. The C `db_match_keeps_key` check is **static** (it rejects a body
  that is not syntactically `'$_'`/`{{ … }}`/`{const,{…}}`/`{element,KeyPos,'$_'}` even when nothing
  matches); this crate validates the *value*, so an exotic multi-expression body diverges (Known
  Issues).
- **Match-spec arithmetic routes through the `ArithHook` seam, not a new crate edge.** `expr::eval`'s
  signature is unchanged (a sibling task owns `exec.rs`); the hook is resolved inside `expr.rs` via a
  `thread_local! { static ARITH_HOOK: RefCell<Option<Rc<dyn ArithHook>>> }` installed by
  `set_arith_hook`. `DefaultArithHook` does checked i64 arithmetic (bignum/float/`/`/overflow ⇒ typed
  `unsupported`, `div`/`rem` by zero ⇒ `badarg`); a real runtime installs a full bignum/float hook.
  Keeps `beam_ets` (wave 1) free of a `beam_bifs` dependency; candidate to lift into `beam_term` later.
- **Continuations are opaque owned values, not terms.** `MatchContinuation` carries table id +
  compiled `MatchSpec` + limit + scan position (`Clone`+`Debug`+`PartialEq`) so `select_continue`
  resumes from the saved position without re-compiling; `Option::None` is the crate's `'$end_of_table'`,
  and the bridge renders the bare-atom vs `{Results,'$end_of_table'}` shape (Known Issues).
- **`match_spec_run` accepts the raw spec.** The oracle requires `ets:match_spec_compile/1` first (a raw
  clause list is `badarg`); this crate has no compiled-spec term, so `run.rs` compiles the raw term
  directly — the bridge maps `ets:match_spec_compile/1` onto the compiled `MatchSpec` (Known Issues).
- **A BODY runtime failure yields the atom `EXIT`; a GUARD failure skips the clause.** The C
  `erl_db_util.c` sets `FAIL_TERM = am_EXIT` around body evaluation (`matchCatch`), so a raising
  last-expression body yields `'EXIT'` and evaluation continues, while a guard error means the clause
  does not match. Verified against `erl` (`[{{'_','_'},[],[{hd,5}]}]` ⇒ `['EXIT','EXIT']`).

## Concurrency, fixation & sharing (current state)
- **`Table` is `Send + Sync` and shareable across processes.** `Table` holds only plain `Eterm` data
  plus a `Box<dyn Interior>` (`pub(crate) trait Interior: Send + Sync`) and a `TermStore` (which
  carries `unsafe impl Send`/`Sync` with a `// SAFETY:` rationale — `beam_alloc::Allocator: Send +
  Sync + 'static`, and every write path takes `&mut self`). `pub struct SharedTable`
  (`src/table/shared.rs`) wraps `Arc<RwLock<Table>>` with `new`/`id`/`ref_count`/`read`/`write`/
  `with_read`/`with_write`/`try_into_inner` + `Clone`/`Debug`. A poisoned lock yields the **typed**
  `unsupported_ets: table lock poisoned` error — never a panic, never a wrong result. Holding one
  `write()` guard across a check-then-act makes `insert_new`/`update_counter`/`select_replace` atomic,
  so concurrent writers lose no updates. (Fine-grained `{write_concurrency}` affects throughput only,
  never observable results.)
- **`ets:safe_fixtable/2` fixation = deferred reclamation.** `Table::set_fixation(bool)` /
  `is_fixed()` / `deferred_objects()`: while fixed, every *removed* object's storage is pushed onto
  `deferred_frees` instead of being freed, and `delete_all_objects` defers each root (rebuilding the
  interior) rather than `reset()`ing the arena; unfixing flushes the deferred frees. So
  `ets:info(T, memory)` is held while fixed and *falls* at unfix, matching the oracle. `deferred_frees`
  is deliberately **not** counted in `memory()` (table-owned, not live objects).
- **Access enforcement covers the newer ops.** `TableAccess` (`src/table/access.rs`) gates
  `select_replace` as a *write* and every `/3` + `/1` limited/continuation form as a *read*
  (`select_limit`/`select_continue`/`match_limit`/`match_continue`/`match_object_limit`/
  `match_object_continue`).
- **Subterm sharing (the copy-boundary trim).** `TermStore::copy_term` shares exactly the one literal
  whose liveness the crate can prove — the global empty tuple (`tuple0()`; C `copy.c:130-140`).
  `beam_term::literal::LiteralArea` literals are **deep-copied** (the area can be dropped and the
  crate holds no handle, so the C's `in_literal_purge_area` guard cannot be honoured). `{keypos, N}`
  keys live inside the stored tuple (no separate key copy); `compressed` is parsed/reported with no
  storage effect (honest gap, see Known Issues).
- **New evidence:** `tests/ets_shared.rs` (5 tests — `Send`/`Sync`, 8-writer no-lost-update, reader
  consistency, owner-only writer under concurrency, cross-thread move), `tests/ets_fixtable.rs` (5 —
  fix/unfix idempotence, storage held while fixed and released at unfix, `delete_all_objects` under
  fixation, ordered walk-and-delete under fixation, hash anchor still required when fixed),
  `tests/ets_access.rs` (+2 — the `select_replace`/limited-form gates), `src/store/tests.rs` (+1 — the
  empty-tuple literal is shared, not copied).

## Test Strategy
- Per-module `#[cfg(test)] mod tests` in each `src/` module (`store/tests.rs` holds `TermStore`'s,
  split out to keep `store/mod.rs` small): kind/options, key validation, store copy/free/reuse and
  `relinquish` (check-out), `Table`-level insert/take store-word accounting, hash-interior ops
  (incl. the `insert`-retained-bool), CHAMP ops (round-trip, `delete_object` non-match, 2000-key
  churn, collision-node fallback, index-collision descend, canonicalization-by-inlining, `memory`
  monotonicity), B+-tree invariants (incl. 10 000-key churn), the sorted-`Vec`-interior ops
  (round-trip, replace/delete, navigation of present/absent keys, `lookup_element` bounds,
  `memory` monotonicity), the B+-tree **mixed-class** order test (`navigates`/iteration over
  integers incl. negative and zero, an atom, a tuple, `[]`, a list and a binary in exactly
  `beam_term::compare::cmp_arith` order, checked for both ordered interiors — never against the live
  oracle), select compile/execute, info. **126 lib unit tests.**
- **Cross-interior equivalence + the selection defaults** (`src/interior_parity.rs`, `#[cfg(test)]`):
  `champ_is_the_documented_default_set_interior` pins `SetInterior::default() == Champ` and
  `bplus_tree_is_the_documented_default_ordered_interior` pins
  `OrderedInterior::default() == BPlusTree`; the chained/CHAMP interiors are driven through a shared
  LCG-generated operation script (insert / `insert_new` / `lookup` / `member` / `lookup_element` /
  `update_counter` / `update_element` / `delete` / `delete_object` / `take` / iteration) and must
  produce **identical** traces for `size` (hence `ets:info(T,size)`), membership, element reads, and
  `select`/`match` iteration *content* (order-insensitive, since `set` navigation order is
  unspecified), at `keypos` 1 and 2. The two **ordered** interiors run the *same* script but with an
  **order-sensitive** tail (`tab2list`, the `first`/`next`-chain walk, the `prev`-chain, `last` and
  `match_object`, all rendered in order) so any ordering divergence is caught, at `keypos` 1 and 2;
  `the_ordered_trace_pins_the_ascending_order` proves that tail really carries order (a reversed run
  would differ); `ordered_interiors_agree_on_arithmetic_key_order` pins the `cmp` → `cmp_arith` change
  on both ordered interiors at once — the `{1,a}`/`{1.0,b}` pair collapses to one key (size 1, last
  object wins) and the numeric mixed keys iterate in value order (`-3 < -2.5 < 0 < 1.0 < 1.5 < 5`).
- **Interior microbenchmarks** (`src/interior_bench.rs`, `#[cfg(test)]`): chained vs CHAMP
  (`interior_microbenchmark`) and B+-tree vs sorted `Vec` (`ordered_interior_microbenchmark`), both
  with in-line `size`/hit/miss/iteration count assertions (so a wrong result can never be timed as a
  fast one). The ordered one drives its keys in a deterministic shuffled order (ascending order would
  flatter the sorted `Vec`). Run with
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets --lib
  interior_microbenchmark -- --nocapture` (or `ordered_interior_microbenchmark`); sizes default to
  `10,1000,100000` and are overridden via the `BEAM_ETS_BENCH_SIZES` (comma list) and
  `BEAM_ETS_BENCH_SHAPES` env vars. The ordered benchmark is the crate's slow one (the sorted `Vec`
  is `O(n)` per mutation) — pass `BEAM_ETS_BENCH_SIZES=10,1000,10000` when iterating.
- Crate-level integration over the **public API only**, split by concern: `tests/ets_basic.rs`
  (incl. `take` reclamation + churn), `ets_order.rs`, `ets_select.rs`, `ets_bag.rs` (incl. the
  duplicate-insert / `insert_new` copy-leak checks), `ets_info.rs` (incl.
  `entries_emits_the_oracle_field_set_in_order`), `ets_nav.rs` (incl. the order-independent
  hash-navigation contract), `ets_access.rs`, `ets_update.rs`, `ets_ownership.rs` (the
  `give_away`/`setopts`/`heir` semantics + the pid-gap rendering), `ets_replace.rs` (the
  `select_replace` semantics incl. the recorded static-vs-value key-check divergences),
  `ets_continuation.rs` (the limit/continuation chaining, incl. a live-oracle cross-check),
  `match_spec_run.rs` (the table-independent runner with oracle cross-checks), sharing helpers from
  `tests/common/mod.rs`; plus `tests/select.rs` for the match-spec compiler + arithmetic.
- **Differential evidence suite** (the compatibility gate): `tests/differential.rs` drives 94 cases
  (group files `tests/differential/battery/{kind,keypos,navigation,counter,lookup,matchspec,replace,
  continuation,info,large}.rs`) identically under the **system `erl`** (OTP 29 / erts-17.1, the
  embedded program in `tests/differential/erl_program.rs`) and under `beam_ets`, comparing canonical
  renderings line-by-line (atoms/small ints/tuples/proper lists; order-insensitive values sorted;
  table ids/`memory`/pids/floats/binaries never rendered). Coverage: all 4 kinds × 8 core ops,
  `{keypos,2}`, ordered navigation, `update_counter/3,4` + `update_element`, `take`/`lookup_element`,
  the select/match family with guards, malformed specs, >10 000-key tables, an `info`/option group
  (name vs `named_table`, the `ets:info/2` option fields incl. the `private` concurrency flags and
  unknown-key `badarg`, the fixed `heir`/`node`/`decentralized_counters` fields, the multi-kind
  `lookup_element` list, atomic `insert_list`, and the ownership `give_away`/`heir`/`holder` cases),
  plus `match_spec_run` (bindings, repeated vars, multi-clause, empty/`badarg`, body `EXIT`),
  `select_replace` (small-int rewrite, arity-1 result, `bag`/changed-key `badarg`, identity) and the
  limit/continuation chains (`select`/`match`/`match_object` `/3`+`/1`, `limit 0` `badarg`, empty
  table). `KNOWN_DIVERGENCES` is empty. Without `erl` the live-oracle test skips;
  `BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into a failure (use it in CI).
- **Gates (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets` and
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --workspace`.
  Forced-oracle: `BEAM_ETS_REQUIRE_ORACLE=1 erts_rust/scripts/cargo.sh test --manifest-path
  erts_rust/Cargo.toml -p beam_ets --test differential`.

## Known Issues (honest gaps — not full parity)
- **`Concurrency` is advisory.** `read_concurrency`/`write_concurrency` are parsed and reported but
  there is no locking or per-reader table; a `Table` is single-threaded.
- **`ets:info/1` `owner`/`heir` are `undefined` and `id` is a small integer.** `TableInfo::entries` emits the
  oracle's full 15-field order, but `owner` — a pid in the oracle — has no term representation at this
  stage, so it renders as the atom `undefined`, and `id` renders as a small integer rather than a ref.
  The `heir` field follows the same convention: `none` when unset, `undefined` when a heir is installed
  (its structured `TableInfo::heir` still carries the pid). **Handoff:** the *observable* `ets:info/1`
  proplist is produced by the sibling `beam_bifs` bridge
  (`src/ets.rs`, its own `INFO_ITEMS` + `info_field`), which still emits the older 12-field subset in a
  different order; it must adopt this completed `entries` (or extend `INFO_ITEMS`/`info_field` to the
  15-field oracle set and order). This crate does not own `beam_bifs`.
- **Ownership modelling has no pid/liveness concept.** `ets:give_away/3` treats a `None` new owner as
  the oracle's non-pid/dead-pid target (both `badarg`); an *unknown table id* is the registry/BIF
  layer's concern, not this crate's. The `{'ETS-TRANSFER', Tab, FromPid, GiftData}` message the new
  owner receives, and the heir hand-off when an owner dies, are the BIF/runtime layer's job — this
  crate models only the ownership/heir *state*.
- **`ets:setopts/2` is modelled only for `{heir, ...}`.** The other `setopts/2` options
  (`{heir, Pid}` arity-2, `{heir, Pid, Data}` duplication-checking, `safe_fixtable`, `{write_concurrency, _}`,
  …) are out of M1 scope; the crate exposes the `{heir, Pid, Data}` / `{heir, none}` subset via
  `SetOpt`. `ets:info(T, holder)` has no `holder` item (the oracle is `badarg`) and none is emitted.
- **Scalar `lookup_element` is unsupported for the multi kinds.** `Table::lookup_element`/
  `lookup_element_or` raise `unsupported_ets` for `bag`/`duplicate_bag` (the oracle returns a *list*
  there); the faithful form is `lookup_element_all`/`lookup_element_all_or`. **Handoff:** the
  `beam_bifs` `ets:lookup_element` bridge must route the multi kinds through `lookup_element_all` and
  build the list.
- **`tab2file`/`file2tab`/`tabfile_info`/`tab2file`-family are BLOCKED — not merely out of scope.**
  Implementing them inside this crate is impossible without widening its dependency edges, because the
  ETS table-file format is `{Header, Objects, Footer}` where **every** field is an External-Term-Format
  term (`$TAB2FILE`-style magic, an `{version, …}`/`{info, …}` header, the objects, and a
  `{hfile, Size, MD5}` footer). Rendering and parsing those terms needs a term codec, which lives in
  `beam_codec` — a **new cross-crate dependency edge** (`beam_ets` is wave 1, `beam_codec` is wave 1,
  and the layer DAG currently has no `beam_ets → beam_codec` edge). Writing/reading the file itself
  needs a file descriptor / I-O, which lives in `beam_io`/`std::fs`; `beam_io` is wave 5, so that is a
  second new edge **in the wrong direction** (a wave-1 crate may not depend upward). Both edges are
  forbidden by the crate-layer contracts (`../CONTEXT.md` *No duplication across crates* / the wave
  table) without explicit parent approval. `ets:tab2file/3` also has oracle behaviours beyond I-O
  (`{extended_info, …}`, the `sync`/`sync_verbose` modes, the ~10 000-object chunking, and re-raising
  the caller's `badarg` for a non-tuple object) that would need their own modelling. Verdict: **not
  implemented**, blocked on (a) a `beam_codec` encode/decode edge for the table-file terms and (b) a
  file-I/O edge — both are cross-crate decisions owned by `../` (the crate layer), not by this crate.
  The same reasoning blocks `file2tab`/`tabfile_info`; `tab2list` + `first`/`next` remain the
  supported enumeration path.
- **Several `ets` operations are outside M1 scope** (all present in OTP 29): `update_element/4`,
  `select_reverse/*`, `foldl`/`foldr`, `rename/2`, `whereis/1`, `table/1,2`, `slot/2`,
  `safe_fixtable/2`, `tab2file*`/`file2tab*`. (`ets:clear/1`, `ets:size/1` and
  `ets:match_spec_test/2,3` do **not** exist in OTP 29 — calling `match_spec_test` raises
  `error:undef` — so their absence is correct; `ets:info(T, size)` is the supported spelling of the
  size query.) The `/3` limited forms are implemented but their opaque **continuation terms** and the
  bare-vs-`{Results,'$end_of_table'}` shaping are the `beam_bifs` bridge's (see the handoff below).
- **`match_spec_run` accepts a RAW spec, unlike the oracle.** The oracle's `ets:match_spec_run/2`
  requires a spec already compiled by `ets:match_spec_compile/1` (a raw clause list is `badarg`);
  this crate has no compiled-spec *term*, so `run.rs` compiles the raw spec and the bridge must map
  `ets:match_spec_compile/1` onto the compiled `MatchSpec`.
- **Named-table lookup-by-name is the runtime's job, not this crate's.** `Table` exposes `name()`/
  `is_named()` only; the name→table registry is the sibling `beam_bifs::ets::TableRegistry` (this
  crate holds no global state, so it must not duplicate that registry).
- **Deep-copy is a subset.** Funs, hash-maps, records, external pid/port/ref and off-heap bin
  refs/sub-bitstrings cannot be copied into table storage (`insert` ⇒ `BadArg`).
- **The match-spec compiler covers the common guard set**, not the full Erlang guard language
  (`map_get`, `float`/`trunc`/`round`, `list_to_tuple`, comprehensions in bodies, …); anything
  outside the recognised set is `badarg`. Match-spec **arithmetic** is covered by the default
  `ArithHook` for small integers only: bignum/float operands and `/` (a float result) return a typed
  `unsupported`; a real runtime installs a full hook via `set_arith_hook`.
- **`select_replace/2`'s key check is value-based, not the C's static syntactic check.** The oracle
  (`db_match_keeps_key`) rejects a body that is not syntactically `'$_'`/`{{ … }}`/`{const,{…}}`/
  `{element,KeyPos,'$_'}` even when nothing matches; this crate accepts any body whose *value* keeps
  the key, so it diverges on exotic multi-expression bodies (pinned by the `recorded_divergence_*`
  tests in `tests/ets_replace.rs`). Wherever the oracle accepts, the results agree.
- **`ets:slot`-style iteration and `tab2file`/`file2tab` are not implemented** (out of scope for this
  pass); `tab2list` plus `first`/`next` cover enumeration.
- **The interior splits are deliberate, and the `lib.rs`/`Cargo.toml` docs state them.** `set`
  defaults to the CHAMP index (`src/champ_interior.rs`); `bag`/`duplicate_bag` use the chained index
  (`src/hash_interior.rs`: `Vec<Vec<Entry>>`, 8 buckets doubling past a 3/4 load factor); the chained
  index stays selectable for `set` (`SetInterior::Chained`). CHAMP is a measurement-driven choice, not
  a like-for-like win: it loses 1.6–2.8× on `delete` and (on small-int keys) up to ~2.1× on lookup
  *misses* — see the Design Decisions bullet and `src/interior_bench.rs` for the numbers. `beam_term::map`'s
  HAMT is a separate layout (for map *terms*, not ETS). `ordered_set` likewise defaults to the B+-tree
  (`src/ordered_interior/mod.rs`) with the sorted `Vec` (`src/ordered_interior/sorted.rs`) selectable
  via `OrderedInterior::Sorted`; there the sorted interior *wins* lookup/iterate/memory but loses
  `O(n)` on insert/delete, so the B+-tree is the default (Design Decisions). The interior shapes are
  unpinned (crate-layer §2), so no contract is affected.
- **`cmp_arith`'s big-integer↔float key comparison is a best-effort `f64` approximation** (a
  pre-existing `beam_term` gap, not introduced here). ERTS's arithmetic key order (`erl_db_tree.c`
  `cmp_key_eq`) compares an integer key against a float key numerically; `compare::cmp_arith` does so
  through `number::number_as_f64`, which rounds an unrepresentable bignum magnitude to `±inf`. This
  only affects an `ordered_set` whose keys mix an out-of-`f64`-range bignum with a float; every
  representable integer/float pair (including the small-int/float keys the tests pin) is exact.
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, `SetInterior` + `OrderedInterior` (the interior selectors), `Access`, `Concurrency`, `TableOptions`, `can_read`/`can_write`
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store/mod.rs` → `TermStore` (table-owned arena + copy/free/reuse) — the copy boundary
- `src/store/tests.rs` → the `TermStore` unit-test module
- `src/table/mod.rs` → `Table`/`TableId`/`OwnerId`, constructors (`new`/`with_allocator`/`with_set_interior`/`set_interior`/`with_ordered_interior`/`ordered_interior`), metadata/ownership accessors, write ops (insert/delete), access checks, `select_all`, module wiring
- `src/table/interior.rs` → the private `pub(crate) trait Interior` + `build_interior(kind, set_interior, ordered_interior)`
- `src/table/read.rs` → the read/navigation `impl Table` (lookup/member/lookup_element*/first/last/next/prev/take/size/is_empty/tab2list/memory)
- `src/table/access.rs` → `TableAccess` (access-enforcing view)
- `src/table/ownership.rs` → `Heir`/`SetOpt` + the `ets:give_away/3` / `{heir, ...}` `setopts/2` semantics + the pid-gap docs
- `src/table/element.rs` → the table-level `ets:update_counter/3,4` + `ets:update_element/3` operations
- `src/table/update.rs` → `update_counter`/`update_element` operand machinery (shared with the interiors and `element.rs`)
- `src/champ_interior.rs` → `ChampInterior` (CHAMP open-addressing index) — the **default `set`** interior
- `src/hash_interior.rs` → `HashInterior` (chained bucket index) for `bag`/`duplicate_bag` (and opt-in `set` via `SetInterior::Chained`)
- `src/interior_parity.rs` → cross-interior equivalence tests (set + ordered) + the default-selection tests (`#[cfg(test)]`)
- `src/interior_bench.rs` → the chained-vs-CHAMP and B+-tree-vs-sorted-`Vec` interior microbenchmarks (`#[cfg(test)]`)
- `src/ordered_interior/mod.rs` → `OrderedSetInterior` (B+-tree) for `ordered_set` + its `Interior` impl
- `src/ordered_interior/sorted.rs` → `SortedOrderedInterior` (sorted-`Vec` interior, opt-in via `OrderedInterior::Sorted`) + its `Interior` impl
- `src/ordered_interior/node.rs` → node layout, `ORDER`/`MIN_OCCUPANCY`, `NodeStore` arena, the shared `Entry`/`search_entries`/`words_of`
- `src/ordered_interior/tree.rs` → B+-tree insert/lookup/delete + rebalance
- `src/ordered_interior/iter.rs` → leaf-chain ordered iterator
- `src/select/mod.rs` → `MatchSpec`/`MatchClause`/`SelectPredicate` + `compile`
- `src/select/pattern.rs` → head patterns (`Pattern`)
- `src/select/expr.rs` → guard/body expression language (`Expr`/`CmpOp`/`TypeTest`) + arithmetic (`ArithOp`/`ArithHook`/`DefaultArithHook`/`set_arith_hook`)
- `src/select/exec.rs` → `impl Table` select family (`select`/`select_delete`/`select_count`/`match*`/`select_replace`) + `BodyKind`
- `src/select/run.rs` → table-independent `match_spec_run` (`ets:match_spec_run/2`)
- `src/select/continuation.rs` → `MatchContinuation` + the limit/continuation forms (`select_limit`/`select_continue`/`match_limit`/`match_continue`/`match_object_limit`/`match_object_continue`)
- `src/info.rs` → `TableInfo` + `entries` (the 15-field `ets:info` pairs, oracle order)
- `src/error.rs` → `badarg`/`unsupported` failure constructors (no new error enum)
- `tests/common/mod.rs` → shared integration-test helpers (one home, no duplication)
- `tests/ets_*.rs` → public-API integration tests by concern (basic/order/select/bag/info/nav/access/update/replace/continuation)
- `tests/select.rs` → the match-spec compiler + arithmetic integration tests
- `tests/match_spec_run.rs` → the table-independent `ets:match_spec_run/2` runner tests
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
