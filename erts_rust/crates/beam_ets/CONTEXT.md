# `beam_ets` — ETS Tables (Stage 3 data structures)

## Intent
Owns the **observable semantics of `ets:*`** — Erlang Term Storage — over **flat, table-owned term
storage** (the A.1 "copy boundary"). Implements Part I §A.1 (term/key/value semantics) and §A.8
(`ets:info/1` introspection) and the Stage-3 row of the plan (Part II §H): `set`, `ordered_set`,
`bag`, `duplicate_bag` with hash-indexed and ordered interiors.

The **interior data structures are NOT externally pinned** (crate-layer `../CONTEXT.md` §2): bucket
layout, hash-map thresholds, tree fan-out and load factors are implementation choices. Only the
**semantics** are contract — which object wins on a duplicate key, key equality/ordering, `size`, and
what `lookup`/`select`/`tab2list` return. Term **ordering** and **hashing** are owned by `beam_term`
(`compare::cmp`/`eq`, `hashing::term_hash`) and MUST NOT be re-implemented here.

Legacy ground truth (read-only): `../../erts/emulator/beam/erl_db.c` (API), `erl_db_hash.c`
(hash tables: `set`/`bag`), `erl_db_tree.c` (`ordered_set`), `erl_db_util.c` (key/term handling, the
match compiler), headers `erl_db.h`/`erl_db_hash.h`/`erl_db_tree.h`. Mirror the SEMANTICS, not the
layouts.

## Position & dependencies
Crate-layer **wave 1** (`../CONTEXT.md`). Depends only on `beam_common` (errors), `beam_term` (terms
+ ordering/hashing), `beam_alloc` (table-owned allocation) and `libc`. Consumed by `beam_bifs` (the
`ets:*` BIFs), `beam_rt`, and (later) `beam_dist`/`beam_message` (table sharing). No new dependency
edges may be added without reporting to `../`.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_ets::prelude::*`.

**The atom table is a per-call parameter.** Every op that must compare or hash a term takes
`atoms: &AtomTable`. There is one live, global atom table in the runtime (`beam_rt` owns it); a
`Table` does not own an `AtomTable` (indices must stay global) and stores no atom state.

### `kind.rs` — kinds, access, options
- `pub enum TableKind { Set, OrderedSet, Bag, DuplicateBag }` (`Default = Set`);
  `as_atom_name() -> &'static str` (`"set"`/`"ordered_set"`/`"bag"`/`"duplicate_bag"`),
  `is_multi() -> bool` (bag/duplicate_bag), `is_ordered() -> bool` (ordered_set).
- `pub enum Access { Public, Protected, Private }` (`Default = Protected`); `as_atom_name()`.
- `pub enum Concurrency { None, Read, Write, ReadWrite }` — **M1 skeleton**, declared for the A.6
  surface; not enforced (see Known Issues).
- `pub struct TableOptions` — the parsed `ets:new/2` option set (builder pattern):
  `TableOptions::new() -> TableOptions` (defaults: `Set`, keypos 1, `Protected`, not named, not
  compressed, no concurrency); `set_kind`, `set_keypos(&mut self, usize) -> Result<&mut Self>`
  (rejects 0), `set_access`, `set_named(bool)`, `set_name(Atom)`, `set_compressed(bool)`,
  `set_write_concurrency(bool)`, `set_read_concurrency(bool)`; getters `kind()/keypos()/access()/
  is_named()/name()/is_compressed()/write_concurrency()/read_concurrency()`; `impl Default`.

### `key.rs` — object/key handling (the DB key rules)
- `pub fn key_of(object: Eterm, keypos: usize) -> Result<Eterm>` — an ETS object **must be a tuple**
  with `arity >= keypos`; returns `element(keypos, object)`. Non-tuple / short arity / `keypos == 0`
  ⇒ `Err(BeamError::BadArg)`.
- `pub fn is_valid_object(object: Eterm, keypos: usize) -> bool` — non-erroring predicate.
- `pub unsafe fn object_elements(obj: Eterm) -> &[Eterm]` — the tuple **payload slice** (no header)
  handed to `select` predicates. `# Safety`: `obj` must be a live tuple; the slice borrows storage
  owned by the table/original heap for the duration of the borrow.

### `store.rs` — table-owned term storage (the copy boundary)
- `pub struct TermStore` — a **never-relocating bump arena** built on an
  `beam_alloc::AllocRef` (`AllocType::Ets`): keeps a chain of owned allocation blocks and hands out
  stable word pointers. `TermStore::new(alloc: AllocRef) -> TermStore`;
  `TermStore::with_default_allocator() -> TermStore` (uses `beam_alloc::allocator(AllocType::Ets)`);
  `copy(&mut self, root: Eterm) -> Result<Eterm>` — deep-copies the term graph into table-owned
  memory and returns the new root (immediates are returned unchanged, copying by value);
  `words() -> usize`, `blocks() -> usize`. `impl Drop` frees every block via the allocator.
- Deep-copy **supported subset** (M1): immediates (by value), cons/list cells, tuples,
  **flat maps**, and the *pointer-free* boxed leaves (float, bignum, internal ref, on-heap
  bitstring). **Unsupported** (⇒ `Err(BeamError::BadArg)`, documented gap): funs, hash-maps,
  records, external pid/port/ref, off-heap bin refs/sub-bitstrings. See Known Issues.

### `table.rs` — the `Table` handle + ops
- `pub struct TableId(u64)`: `new(u64)`, `raw(self) -> u64`, `next() -> TableId` (process-global
  monotonic allocator), `Display`. `pub struct OwnerId(u64)`: `new(u64)`, `raw(self) -> u64` — the
  owning process id (M1 **skeleton**; access is not yet enforced by owner).
- `pub struct Table { ... }` — metadata + a table-owned `TermStore` + one interior:
```rust
impl Table {
    pub fn new(id: TableId, opts: TableOptions) -> Result<Table>;
    pub fn with_allocator(id: TableId, opts: TableOptions, alloc: AllocRef) -> Result<Table>;

    // metadata
    pub fn id(&self) -> TableId;
    pub fn kind(&self) -> TableKind;
    pub fn name(&self) -> Option<Atom>;      // Some iff named_table
    pub fn keypos(&self) -> usize;           // 1-based
    pub fn access(&self) -> Access;
    pub fn owner(&self) -> Option<OwnerId>;
    pub fn set_owner(&mut self, owner: Option<OwnerId>);
    pub fn info(&self) -> TableInfo;

    // writes (object = the whole tuple; key derived from keypos)
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()>;
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool>; // false if key exists
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()>;
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()>;         // ets:delete/2 (by key)
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()>;
    pub fn delete_all_objects(&mut self);

    // reads
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm>;             // 0/1 for set/ordered_set
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> bool;
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm>; // badarg if absent
    pub fn size(&self) -> usize;                                                  // objects (not keys) for bag/dup_bag
    pub fn is_empty(&self) -> bool;
    pub fn tab2list(&self) -> Vec<Eterm>;                                         // ordered for ordered_set

    // select
    pub fn select_all(&self, pred: &mut SelectPredicate<'_>) -> Vec<Eterm>;        // the M1 primitive
    pub fn select(&self, spec: &MatchSpec, atoms: &AtomTable) -> Result<Vec<Eterm>>; // M1 subset
}
```
- `pub(crate) trait Interior` — the internal dispatch seam implemented by the two interiors
  (`insert`/`insert_new`/`delete`/`delete_object`/`lookup`/`member`/`lookup_element`/`size`/
  `iter_objects`); `Table` stores `Box<dyn Interior>`. Private — **not** part of the cross-crate API.

### `hash_interior.rs` — hash-indexed interior (`set`/`bag`/`duplicate_bag`)
- `pub struct HashInterior` — buckets `Vec<Vec<Entry>>` where `Entry { key: Eterm, objs: Vec<Eterm> }`;
  `Entry` found by `term_hash(key) % buckets` then `eq`; grows (doubles + rehashes) past a load
  factor. `set` keeps ≤ 1 object per key; `bag`/`duplicate_bag` keep many (`bag` rejects an identical
  object within a key via `eq`, `duplicate_bag` does not). `HashInterior::new(kind: TableKind)`.
  Public for testing/reuse, not the cross-crate seam.

### `ordered_interior.rs` — ordered interior (`ordered_set`)
- `pub struct OrderedSetInterior` — a **key-sorted `Vec<Entry>`** (`Entry { key, obj }`), binary-searched
  by `compare::cmp`. Insert is `O(n)` (Vec shift); sufficient and correct for M1 (a B-tree is a
  documented future optimization). `OrderedSetInterior::new()`.

### `select.rs` — selection
- `pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;` — receives the object's tuple
  **element slice**; returns "keep".
- `pub struct MatchSpec { pub clauses: Vec<MatchClause> }`,
  `pub struct MatchClause { pub head: MatchPattern, pub guard: MatchGuard, pub body: MatchBody }` —
  the **M1 match-spec subset**: head = tuple of literals / `'_'` (wildcard) / `'$N'` (bind element N);
  guard = empty only; body = tuple of literals / `'$N'` / `'$_'` (whole object).
  `pub fn compile(spec: Eterm, atoms: &AtomTable) -> Result<MatchSpec>` — decodes that subset from the
  term form; anything outside it ⇒ `Err(BeamError::BadArg)`.

### `info.rs` — introspection (`ets:info/1`)
- `pub struct TableInfo { pub id, pub name: Option<Atom>, pub kind, pub keypos, pub access,
  pub size, pub owner: Option<OwnerId>, pub compressed: bool, pub write_concurrency: bool,
  pub read_concurrency: bool }`; `pub fn entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)>`
  — the `{Key, Value}` proplist pairs (`id`/`name`/`type`/`keypos`/`protection`/`size`/`owner`/…) as
  `(atom, value)` pairs; the BIF layer turns them into the `ets:info/1` list. Heap-free (no term
  construction here).

### `error.rs` — failure constructors
- `pub(crate) fn badarg() -> BeamError` (and any sibling helpers). No crate-local error enum — the
  workspace error type (`beam_common::BeamError`) is used exclusively.

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`. `badarg`-style failures ⇒ `BeamError::BadArg`;
  never `panic!`/`unwrap()` on runtime data; no crate-local error enum. Table ops never panic on
  malformed input (bad object, bad keypos, missing element) — they return `Err`.
- **Never re-implement term order/hash.** Ordering ⇒ `beam_term::compare::cmp`/`eq`; hashing ⇒
  `beam_term::hashing::term_hash`. Duplicating either is a structural defect (workspace Prevent-
  Duplication rule).
- **Table-owned storage only (A.1 copy boundary).** An inserted object is **deep-copied** into the
  table's `TermStore`; the table never retains a pointer into a caller's/process heap. Objects/key
  returned from reads alias table storage and stay valid for the table's lifetime.
- **`unsafe` confinement.** `unsafe` is confined to `store.rs` (raw term reads/writes), the
  tuple-element accessor in `key.rs`, and the small term-reading blocks in `select.rs::compile` and
  `table.rs::select_all`/`select` — `beam_term` exposes *every* tuple/list accessor (`is_tuple`,
  `tuple_size`, `tuple_element`, `car`, `cdr`, …) as an `unsafe fn`, so reading a caller-owned spec
  term or a stored tuple's element slice needs a confined block. Every `unsafe fn` carries a
  `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`beam_common::safety` convention); `unsafe_op_in_unsafe_fn` stays clean (workspace lint).
  `kind`/`key` (validation)/`hash_interior`/`ordered_interior`/`info`/`error` and the rest of
  `table.rs` are **safe Rust**.
- **Files ~1000 lines**, one responsibility per file.
- **Licensing** Apache-2.0; no new external deps (only `beam_common`/`beam_term`/`beam_alloc`/`libc`).

## Design Decisions
- **Interiors are a private `Interior` trait, not a pinned layout.** The workspace §2 rule says the
  interior is open; a private trait (`Vec`-of-buckets hashing; key-sorted `Vec`) lets the shape change
  without touching `Table` or any dependent. A future CHAMP/HAMT/B-tree interior slots in behind it.
- **One `TermStore` per table; copy-on-insert.** Term graphs are deep-copied into a never-relocating
  bump arena over a `beam_alloc` `AllocRef` (`AllocType::Ets`), so handed-out interior pointers never
  dangle across growth. Immediates copy by value. This is the A.1 copy boundary realized honestly —
  **not** a `beam_message::copy_struct` reuse (that crate is wave 2; `beam_ets` is wave 1 and cannot
  depend upward). A later convergence can route both through the shared copy engine.
- **`&AtomTable` is passed per call.** A `Table` cannot own atom state (indices are global and owned
  by the runtime); threading `&AtomTable` matches `beam_term`'s `cmp`/`eq`/`term_hash` signature and
  keeps `Table` free of a lifetime and of atom lifetimes.
- **`ets:size/1` counts objects, not keys.** For `bag`/`duplicate_bag` a key may hold many objects;
  `size` returns the object count (matching the BIF), which differs from the bucket/entry count.
- **`set`/`ordered_set` are the M1 gate; `bag`/`duplicate_bag` are implemented over the shared hash
  interior but secondary.** Both are cheap once `HashInterior` exists (`objs: Vec<Eterm>` per key +
  an `eq`-based identity check only for `bag`).
- **`lookup` returns objects that alias table storage.** The runtime layer (`beam_bifs`) copies them
  out to the caller's heap; `beam_ets` does not own a target heap. Documented (Known Issues).

## Test Strategy
- Per-module `#[cfg(test)] mod tests`:
  - `kind`: option parsing/defaults, `keypos` rejection of 0, `as_atom_name` round-trips.
  - `key`: tuple/arity validation, `key_of` element selection, non-tuple ⇒ badarg, `object_elements`.
  - `store`: copy of immediates/cons/tuple/flatmap round-trips structurally equal to the source;
    source freed/mutated after copy ⇒ target unaffected; block growth stays non-relocating; `words`.
  - `hash_interior`: insert/replace/lookup/member/delete/delete_object, rehash across many keys,
    `bag` duplicate-object rejection vs `duplicate_bag` allowance, `size`.
  - `ordered_interior`: ordering by `cmp` across classes (number < atom < tuple < list, `1` vs `1.0`
    distinct), binary-search hit/miss, delete.
  - `select`: predicate over element slices; match-spec subset head/body substitution.
  - `info`: `entries` shapes and values.
- `tests/ets.rs` — crate-level integration over the **public API only**: for both `set` and
  `ordered_set`, `insert`/`lookup`/`member`/`delete`/`size`/`tab2list` semantics; key ordering
  (`ordered_set` iterates in `cmp` order) and key equality (`1` vs `1.0` are distinct keys); a
  `select_all` predicate test; `insert_new` false-on-existing; `lookup_element` badarg on missing.
  Objects/key terms constructed with `beam_term` and compared with `beam_term::compare::eq`.
- **Gate (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets` and
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to the sibling `../tools/differential/` (the
  compatibility gate), not to this crate's unit tests.

## Known Issues (M1 gaps — honestly scoped, not full parity)
- **`select` is a subset.** Only the predicate primitive (`select_all`) plus the simplest match-spec
  forms (wildcard `'_'`, element binds `'$N'`, whole-object `'$_'`, **empty guards**) are implemented.
  The full Erlang match-spec compiler (guards `andalso`/`orelse`, `is_*` tests, `element/2`,
  `map_get`, arithmetic, list comprehensions in bodies) is NOT implemented; `compile` rejects it.
  A *body* that builds a fresh tuple is also outside the subset: `select` has no target heap to
  construct a tuple in, so a zero- or multi-element `MatchBody::Tuple` is `badarg`; only
  `'$_'`/a **single-element** `MatchBody::Tuple` (a literal, a `'$N'` or `'$_'`) produces a result.
- **Deep-copy is a subset.** Funs, hash-maps, records, external pid/port/ref and off-heap bin
  refs/sub-bitstrings cannot be copied into table storage yet (`insert` ⇒ `BadArg`). Supported:
  immediates, lists, tuples, flat maps, float/bignum/internal-ref/on-heap-bitstring.
- **No concurrency / access enforcement.** `Concurrency`, `Access`, and `OwnerId` are metadata only;
  M1 has no locking, no per-reader tables, and does not reject cross-owner access. Table sharing is
  single-threaded.
- **Deletion does not reclaim term storage.** A `TermStore` block is freed only when the table is
  dropped, so reads remain valid for the table's lifetime; a delete/re-insert loop grows memory until
  the table is dropped (a per-object free / MSO will address this).
- **`ordered_set` interior is a sorted `Vec`** (`O(n)` insert); no B-tree/CA-tree yet.
- **`info`/`entries` and CLI/`ets:info` string rendering** cover the core fields only; the full
  `ets:info/1` field set and `ets:info/2` are deferred to `beam_rt`.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/kind.rs` → `TableKind`, `Access`, `Concurrency` (skeleton), `TableOptions` (+ builder/parse)
- `src/key.rs` → object/key validation (`key_of`, `is_valid_object`, `object_elements`)
- `src/store.rs` → `TermStore` (table-owned bump arena + the deep-copy subset) — the copy boundary
- `src/table.rs` → `Table`/`TableId`/`OwnerId`, the `Interior` trait, op dispatch + metadata
- `src/hash_interior.rs` → `HashInterior` (bucket hash index) for `set`/`bag`/`duplicate_bag`
- `src/ordered_interior.rs` → `OrderedSetInterior` (key-sorted) for `ordered_set`
- `src/select.rs` → `SelectPredicate` + the `MatchSpec` subset (`compile`)
- `src/info.rs` → `TableInfo` + `entries` (the `ets:info/1` pairs)
- `src/error.rs` → `badarg`/failure constructors (no new error enum)
- `tests/ets.rs` → public-API integration tests (set + ordered_set + select)
- `../../erts/emulator/beam/erl_db.c` → the API + trap-loop semantics being mirrored (read-only)
- `../../erts/emulator/beam/erl_db_hash.c` / `erl_db_tree.c` / `erl_db_util.c` → the interiors + key/
  match handling being mirrored (read-only)
- `../../erts/emulator/beam/erl_db.h` / `erl_db_hash.h` / `erl_db_tree.h` → flags/fields (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the `Table` seam, dependency waves, milestone scope.
- `../../CONTEXT.md` → workspace contracts (§1 A.1/A.8, §2 frozen term layout, §3 error strategy, §7).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`hashing` utilities.
- `../beam_alloc/CONTEXT.md` → the `AllocRef`/`AllocType::Ets` allocation seam `TermStore` uses.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §B/§D → data-structure and ETS copy-trimming discussion.
