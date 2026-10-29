# `beam_term::map` — the incremental (path-copying) map writer

## Intent
The map child modules: the destination-heap, path-copying update writer (the `cow/` cluster) that makes one
`maps:put/3`, `maps:remove/2` or `#{K := V}` cost Θ(log n) words instead of Θ(n), its unit tests
(`cow/tests.rs`), and the relocated in-module unit tests of the flat-map/HAMT builder (`tests.rs`).
The builder itself (`write_map`, `map_words_capacity`, the readers, the from-scratch `map_put`/`map_remove`)
stays in the parent node `src/map.rs` — read its module doc first; the node layouts, the 4-bit nibble
geometry (`HAMT_MAX_LEVEL = 16`, `ihash::hashmap_nibble`) and the flat-key order
(`compare::cmp_flatmap_keys`) are defined there and are **frozen** here too.

Ground truth for every arm: `../../../../erts/emulator/beam/erl_map.c` —
`erts_maps_put` (`:2105-2235`), `erts_hashmap_insert` (`:2477-2504`), `erts_hashmap_insert_down`
(`:2506-2657`), `erts_hashmap_insert_up` (`:2659-2790`), `erts_maps_take`'s flat branch (`:1943-2026`),
`hashmap_delete` (`:2881-3180`); macros at `erl_map.h:200-220`.

## API Surface (re-exported from `src/map.rs`, so dependents keep the `beam_term::map::<name>` path)

```rust
pub unsafe fn map_put_cow(hp: *mut Eterm, map: Eterm, key: Eterm, value: Eterm, atoms: &AtomTable)
    -> TermResult<Eterm>;                                        // `cow/mod.rs`
pub unsafe fn map_remove_cow(hp: *mut Eterm, map: Eterm, key: Eterm, atoms: &AtomTable)
    -> TermResult<Option<Eterm>>;                                // `cow/mod.rs`  (`None` = absent, writes nothing)
pub fn map_put_cow_words_capacity(size: usize) -> usize;         // `cow/words.rs`
pub fn map_remove_cow_words_capacity(size: usize) -> usize;      // `cow/words.rs`
pub fn map_put_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize>;     // `cow/words.rs`
pub fn map_remove_cow_words_required(map: Eterm, key: Eterm) -> TermResult<usize>;  // `cow/words.rs`
```

Both writers refuse a map whose interior this crate's builders cannot produce — a collision node holding
fewer than `HAMT_COLLISION_CELLS_MIN` (2) or more than the corresponding `*_COLLISION_CELLS_MAX` cells, a
flat map whose head word disagrees with its keys tuple's arity, or any other malformed node/child — with
the typed `TermError::BadMap`, **before** the first store. `*_words_required` shares that plan, so it
answers the same error and can never lead a caller into a reservation the write then exceeds.

### Put arms (`plan_put` → `PutAction`, `cow/put.rs`)
| arm | when | words written |
|---|---|---|
| `Nothing` | the stored value **word** is already the new value word (flat: `values[i] == v`; HAMT: `cval == v`, `erl_map.c:2523-2525`) | `0` — the input map word is returned |
| `FlatReplace` | flat map, key present (`eq`, not word equality), different value | `3 + n`, the source map's **keys-tuple word reused verbatim** (the C's `*hp++ = mp->keys`); the keys are never rebuilt or re-sorted |
| `FlatInsert` | flat map, absent key, `n < 32` | `flatmap_size_words(n + 1)`, keys merged at their `cmp_flatmap_keys` position |
| `FlatConvert` | flat map, absent key, `n == 32` — the 33rd key (`erts_hashmap_from_ks_and_vs_extra`) | `map_words_capacity(n + 1)` = **711** planned (`write_map` writes 127) |
| `Hamt` | `size > 32`: copy one node per path level, share every off-path subtree by word | the spine + one new 2-word `[Key, Value]` cons cell |

The HAMT arm reproduces the C exactly: head array / head bitmap / inner bitmap slot arithmetic
(`slot = popcount(bitmap & (bit - 1))`), insertion into an unoccupied nibble, leaf-with-same-key
(`update_size = 0`, head `size` unchanged), leaf-with-a-different-key ⇒ `insert_subnodes`' divergence
chain (`push_divergence`, `cow/put.rs`), collision-node insert (`c+1` cells) and replace (`c` cells, the
changed cell path-copied like the node), and the head-bitmap → `MAP_HEADER_HAMT_HEAD_ARRAY` upgrade when
`hval | bp == 0xffff` (same 18-word layout, pure header rewrite). Inserting a key adds 1 to the head's raw
`size` word; replacing a value does not.

### Remove arms (`plan_remove` → `DelAction`/`Res`, `cow/remove.rs`)
The C's `res == THE_NON_VALUE` state machine, modelled as the **payload-less**
`Res { Removed, Leaf, Node }` (the child term rides the `DelFrame`, not the `Res`):
flat removal writes `flatmap_size_words(n - 1)` in the surviving key order (removal preserves the
canonical order, so no re-sort); the HAMT spine copies with the bitmap change matrix (n=1 keep/remove,
n=2 shrink-or-collapse, n>2 shrink), the `res is a leaf && n == 1 ⇒ keep` case, the head-array →
head-bitmap downgrade `0xffff ^ (1 << ix)` with `size - 1`, collision shrink (`c > 2`) / collapse
(`c == 2`), and the "result size ≤ 32 ⇒ rebuild as a flat map" conversion (`erl_map.c:2946-2981`,
storage order then `cmp_flatmap_keys`). An absent key answers `None` and writes nothing.

### Two word figures — capacity (size-only) vs required (path-derived)
| op | `size < 32` | `size == 32` | `size > 32` |
|---|---|---|---|
| put | `flatmap_size_words(size + 1) ≤ 70` | `map_words_capacity(33) = 711` | **293** |
| remove | `flatmap_size_words(size - 1) ≤ 68` | `flatmap_size_words(31) = 66` | **288** |

Derivation of the HAMT figures (`cow/words.rs`, from the named parts `HAMT_HEAD_WORDS_MAX` (= `2 +
HAMT_MAX_LEVEL` = 18), `HAMT_INNER_WORDS_MAX` (= `1 + HAMT_MAX_LEVEL` = 17), `HAMT_LEAF_WORDS` (2) and
`HAMT_COLLISION_HEADER_WORDS` (1)): the two maxima are built on the deliberately **loose** spine
`HAMT_SPINE_WORDS_LOOSE = 16 · 18 = 288` (every level priced as a head). A put adds a two-cell collision
node (3) and the new leaf's 2-word cons cell (2) ⇒ **293** (`16·18 + 3 + 2`); a divergence chain starting
at `lvl ≤ 15` costs `lvl·18 + (15-lvl)·2 + 5 = 16·lvl + 35 ≤ 275`, always below 290, so only the collision
case sets the bound. A remove writes no cons cell and its flat-conversion arm writes
`flatmap_size_words(32) = 68 < 288` ⇒ **288**. Both figures are **independent of `n`** — that is the whole
point: the reservation does not grow with the map.

**What the two maxima are conditional on.** A hash path's *real* spine is only `HAMT_SPINE_WORDS_MAX =
18 + 15 · 17 = 273` (one head, then 15 inner nodes), and a collision node's cell count `c` is bounded by
the **map's size**, not by path depth — so the true worst cases are `276 + c` (a put: spine + node header
+ `c` cells + the new leaf's cons cell; the insert arm writes `c + 1` cell slots, i.e. `277 + c`) and
`273 + c` (a remove: spine + the shrunk node's `1 + (c - 1)` cells). The size-only figures only cover
`c ≤ HAMT_PUT_COLLISION_CELLS_MAX = 293 - 277 = 16` and `c ≤ HAMT_REMOVE_COLLISION_CELLS_MAX = 288 - 273 =
15`, which is sound for **every map a canonical builder can produce**: a collision node needs two keys
sharing all 64 bits of `erts_map_hash`, and `ihash::erts_map_hash` is a bijection over an immediate's raw
word, so no collision is constructible from ordinary keys. A **hand-forged** wider node is refused with
`BadMap` in the plan phase (nothing is written), which is what keeps the reservation the caller was told
to make a real bound in release mode too — a `debug_assert!` is not a guard. A one-cell collision node is
refused for the same reason (the C's `ASSERT(n >= 2)` is debug-only).

`*_words_required` descends read-only (hashing and nibbles only, no allocation, no `atoms`) and answers
the figure including a `c`-cell collision node's `c + 1` / `c - 1` words, which the size-only bound
cannot express (a collision node's cell count is bounded by the map's size, not by path depth). It is
exact on every map the writer **accepts**, on every arm **except** the flat→HAMT conversion, where it is
`write_map`'s capacity bound; and it refuses the same maps (`BadMap`), so `required ≤ capacity` holds for
every map either form answers for.

## Constraints
- **Node word shapes are frozen here exactly as in `src/map.rs`** — `beam_gc` prices them through
  `layout::object_words` / `for_each_child_slot` (`head array 18`, `head bitmap 2+n`, `inner 1+n`,
  collision `1+n`). Never add a field, a padding word or a per-level bit-geometry change.
- **Key identity is `compare::eq` (exact, `=:=`)** and flat/insertion order is `compare::cmp_flatmap_keys`
  (atoms by index, else `cmp`) for flat maps and trees; collision cells sort by `compare::cmp`. Never
  `cmp_arith`, never a crate-local comparison — `1` and `1.0` are distinct keys.
- **Heap-agnostic and term-allocation-free:** every term write addresses `hp` and is bounded by the
  caller's reservation; host-side `Vec`s (the descent frames, the conversion pair slice) are allowed and
  are the only allocation. No word is written before the arm's last fallible check.
- **`unsafe` confinement:** the writers are `unsafe fn` with a `# Safety` section, every block carries
  `// SAFETY:`, `unsafe_op_in_unsafe_fn` stays clean. The `# Safety` sections state **both**
  preconditions: (a) `hp` is a fresh region of at least the required words in a heap that **outlives the
  source map**, and (b) off-path subtrees and reused keys tuples are shared by word only because terms
  are immutable and never relocated in place.
- **The plan/writer split is load-bearing:** `plan_*` performs the whole read-only descent and decides
  every arm before a single word is written, which is what makes (a) `*_words_required` possible, (b) all
  typed rejections (`TermError::BadMap` for a malformed node/child) happen before any write, and (c) the
  `debug_assert!(out.written() == plan.words)` a real invariant. Keep it: no arm may write while still
  able to reach a fallible check.
- Files ~1000 lines, one responsibility each: the writer is the `cow/` cluster — `cow/mod.rs` (≈365 lines:
  the staging cursor, `flat_pair_count` and the two entry points), `cow/words.rs` (≈190: the word figures,
  their named parts and the refusal thresholds), `cow/node.rs` (≈125: node classification),
  `cow/put.rs` (≈565: the put arm), `cow/remove.rs` (≈500: the remove arm). Grow a new arm as a new child
  module rather than into an existing one.

## Test Strategy
- `cow/tests.rs` (≈1280 lines, 27 cases) — unit level, over-sized `Vec<Eterm>` with a `SENTINEL` tail:
  empty/1-pair, the canonical flatmap header word (a wrong inner header field is invisible to readers, so
  it is pinned explicitly), keys-tuple reuse on the flat replace arm, the zero-write arms
  (same value word, absent remove), front/middle/end flat insertion, `31→32`, `32→33`, HAMT
  put/replace/delete at 33/40/64/250, `remove down to 32 ⇒ flat`, delete every key to zero,
  re-insert past 32, an inner-node divergence chain, a per-key put+remove sweep at 8 sizes, the capacity
  figures, an exactly-`required`-sized reservation with the sentinel tail intact, typed `BadMap`, and a
  fold-of-puts vs one rebuild. Every case checks `map_size`, `map_get` per key and `map_pairs` against a
  `write_map` rebuild.
- **The collision arms are hand-built, because no ordinary key can reach them.** A collision node exists
  only for keys sharing all 64 bits of `erts_map_hash`, and `ihash::erts_map_hash` is a bijection over an
  immediate's raw word, so `cow/tests.rs` assembles the trie word for word in the frozen node layout
  (`Hand::Leaf`/`Hand::Collision`/`Hand::Node`, `hand_words`, `hand_write`, `build_hand_map`) and pins the
  collision insert / value replace (delta 0) / `CollisionShrink` / `Collapse` arms
  against `map_pairs`, `map_size`, `map_get`, the hand-built storage order and the exact
  `*_words_required` figure, plus the two refusals (a wide node, a one-cell node) and a forged flat map
  whose head word disagrees with its keys tuple — each asserting the writer wrote **nothing**. The keys do
  not really collide, so a collision case compares content against a `write_map` rebuild but pins the
  *storage order* against the hand-built trie instead (a rebuild would re-shape the trie).
  **One collision arm stays unreachable even by hand:** `PutFrame::ChainCollision` — the divergence chain
  that ends in a fresh two-cell node — needs the *incoming* key's full `erts_map_hash` to equal a stored
  leaf's, which no constructible key can satisfy. It is covered by inspection, not by a test.
- `tests/map_cow_equivalence.rs` (the crate-level node) is the semantics gate: the op script through both
  routes at 0/1/2/31/32/33/34/64/250/2500 over 11 key classes, a canonical structural rendering, the
  storage-order pair sequence (the `term_to_binary`-bytes stand-in — this crate cannot depend on
  `beam_codec`), the structural/sharing walk, and the flat-in-`n` word pin.
- `examples/map_cow_bench.rs` is the timing home (report only, no wall-time assertions).
- **Owned by another node:** a `beam_gc` copy test over a *partially shared* map — a collection that
  relocates the source and the result across heaps, proving each shared off-path subtree is forwarded
  exactly once and never duplicated. Not writable from this crate.

## Notes for Agents
- **Adoption is still owed (and is the only remaining reason `map_put`/`map_remove` exist for updates
  of an existing map):** `beam_engine::families::maps::write` (`put_map_assoc`/`put_map_exact`),
  `beam_bifs::maps` (`put`/`remove`/`update`/`take`) and `beam_abi::nif::enif::map`
  (`enif_make_map_put`/`_update`) still rebuild. Each must reserve
  `map_put_cow_words_capacity(map_size(map))` / `map_remove_cow_words_capacity(...)` (or the exact
  `*_words_required` figure) and call the writer; `write_map` stays for the from-scratch builders.
- **A caller that stages the result in a disposable arena and then copies it onto the process heap pays
  one extra full copy** — correct but not free, and the reason the adopter should write directly into a
  heap that outlives the source map.
- `*_words_required` uses an empty `AtomTable` internally and that is sound: both `erts_map_hash` (an
  immediate — atoms included — hashes from its raw word) and `compare::eq` (atoms by index) are
  table-free, so the figure cannot take a different path than the writing call. `plan_put` is invoked
  with `value: None` there, so the figure is the writing put's upper bound (no `Nothing` arm).
- A value word that is `=:=`-identical implies term identity even for boxed terms, so the `Nothing`
  short-circuit is safe; do **not** extend it to a deep-equality check of two different words — proving
  deep equality would cost more than the write it saves.

## Routing Table
- `cow/mod.rs` → the cluster root: the module doc (what is copied, what is shared, the two word figures,
  the semantics worth pinning), the `Out` staging cursor, the two public entry points
  `map_put_cow`/`map_remove_cow`, and the re-exports the parent `src/map.rs` publishes
- `cow/words.rs` → the word accounting: `map_put_cow_words_capacity`/`map_remove_cow_words_capacity`
  (size-only bounds), `map_put_cow_words_required`/`map_remove_cow_words_required` (path-derived exact
  figures), the spine-bound constants and their derivation
- `cow/node.rs` → HAMT node classification and slot arithmetic: `Child`/`classify_child`, `node_layout`,
  `slot_of`, `bitmap_header` (the C's `primary_tag` dispatch and the bitmap slot map)
- `cow/put.rs` → the put arm: `plan_put`/`plan_flat_put`/`plan_hamt_put`/`push_divergence` (the read-only
  plan) and `put_frame`/`exec_put` (the writer)
- `cow/remove.rs` → the remove arm: `Res`/`DelFrame`/`DelSpec`/`plan_remove`/`plan_flat_remove`/
  `plan_hamt_remove` (the read-only plan) and `put_del_frame`/`exec_remove` (the writer)
- `cow/tests.rs` → the writer's unit suite (arm-by-arm, sentinel-guarded)
- `tests.rs` → the relocated in-module unit tests of the builder (`write_map`, the readers, the
  from-scratch `map_put`/`map_remove`) — the parent `src/map.rs` owns `#[cfg(test)] mod tests;`
- `../map.rs` → the parent node (flat-map/HAMT layout, `write_map`, `map_words_capacity`, readers,
  rebuild routes) — read its module doc for the frozen layout
- `../../CONTEXT.md` → the crate node; `../../tests/CONTEXT.md`, `../../examples/CONTEXT.md` → the
  integration suites and the measurement binaries

## See Also
- `../../../../erts/emulator/beam/erl_map.c` / `erl_map.h` — the reference algorithm and macros.
- `../../../../CONTEXT.md` §2 (frozen term layout), `../../CONTEXT.md` "Known Issues" (the remaining
  rebuild call sites) and "Handoffs" (what an adopter installs).
