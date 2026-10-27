# `src/atom/store.rs` — the two copy-on-write storage directions

## Intent
Hold the storage behind [`super::AtomTable`] (`../../atom.rs`) so that a whole-table copy can
**share** instead of copy, and so that an append after such a copy touches a bounded slice.
An atom table answers two questions, indexed in opposite directions, and each direction gets its
own copy-on-write container:

- **index → name** (which name does `Atom(i)` denote?) — `NameChunks`, a list of fixed-capacity
  `NameChunk`s. O(1) random access, and an append never renumbers an existing index.
- **name → index** (which index does `"ok"` have?) — `IndexShards`, the map hash-sharded into
  `INDEX_SHARDS` independent shards, so an append copies one shard (about `len / INDEX_SHARDS`
  entries) instead of the whole bucket array.

Each direction sits behind exactly one `Arc`, which is what makes `AtomTable::fork` two `Arc`
clones (zero allocations) instead of a `Vec::clone` plus a `HashMap::clone` (the boot's largest
single attributable RSS region). Every mutating path goes through `Arc::make_mut`, which *copies* a
container rather than writing through one a live fork can still see — that is the isolation the
two sides of a fork are promised, and it is why a fork's view is frozen.

## API Surface
All four groups are crate-private except the two tuning constants, which are `pub` so the
allocation pins can derive their budget instead of hard-coding a slack.

- `pub const NAMES_PER_CHUNK: usize = 128` — names per `NameChunk`; also the `names%` modulus that
  turns an index into `(chunk, slot)`.
- `pub const INDEX_SHARDS: usize = 128` — the shard count; **must** be a power of two (a
  `const` assertion pins it, because `shard_of` masks with `INDEX_SHARDS - 1`).
- `NameChunk { names: Vec<Arc<str>> }` — `with_capacity()`, `push(Arc<str>)` (keeps a full chunk's
  headroom, so a cloned chunk whose capacity is its length does not regrow per name), `len()`;
  test-only `name_arc(slot)`.
- `NameChunks { chunks: Option<Arc<Vec<Arc<NameChunk>>>> }` — `new()`, `fork()`, `len()`,
  `get(index) -> Option<&str>`, `push(Arc<str>) -> u32` (**the only mutating path**, returns the
  index it took), `reserve_names(total)`; test-only `container_ptr()`/`name_arc(index)`.
- `IndexShard { map: HashMap<Arc<str>, Atom> }` — one shard's map (`Clone` is required:
  `IndexShards::insert` detaches a shared shard with `Arc::make_mut`).
- `IndexShards` (`shards: Option<Arc<[Option<Arc<IndexShard>>; INDEX_SHARDS]>>`) — `new()`,
  `fork()`, `get(&str) -> Option<Atom>`, `insert(Arc<str>, Atom)` (**the only mutating path**),
  `entries()` (Σ shard `map.len()`, O(`INDEX_SHARDS`), debug-only use); test-only `container_ptr()`.
- `shard_of(&str) -> usize` — FNV-1a 64 over the name bytes, then the splitmix64 finalizer, masked
  to the shard count.

## Constraints
- **Indices are the observable identity.** `chunks[i][j]` names `Atom(i * NAMES_PER_CHUNK + j)`,
  `push` derives the index from `len()`, and every chunk but the last is full. Nothing may
  renumber, reorder or remove an index.
- **`new()` and `fork()` allocate nothing.** `None` is the empty state for both directions, so an
  empty table costs nothing and a fork is two `Arc` clones at any size.
- **Only `Arc::make_mut` may mutate.** Never `get_mut` through a shared `Arc`, never touch a
  container the other side of a fork can see — that is the whole isolation guarantee.
- **`shard_of` is a pure function of the name bytes** (no seed, no table state, no allocation), so
  a fork and its parent — and any number of fork generations — agree on where a name lives without
  coordinating.
- `NAMES_PER_CHUNK` and `INDEX_SHARDS` are **tuning constants, not contracts**: `NAMES_PER_CHUNK`
  moves the copy granularity, `INDEX_SHARDS` the shard granularity, and no caller may depend on
  either value (the two `pub` constants exist only for the allocation-budget arithmetic).
- No `unsafe`, no panicking path over runtime data: every accessor returns `Option`.

## Cost model (the reason the module exists)
- `fork()` — 0 allocations, O(1).
- The first append after a fork — at most one chunk (≤ `NAMES_PER_CHUNK` handles) and one shard
  (≈ `len / INDEX_SHARDS` entries) are materialized, i.e. O(size of the touched containers), never
  O(table). Each `Arc::make_mut` step is two allocations (the cloned payload + the new `Arc`
  block); measured at **9 allocations** for a fork-then-intern at both 300 and 3 000 atoms
  (`../../../tests/intern_alloc.rs`, case (f), which pins the bound as flat).
- `get`/`len`/`insert` — one shard hash plus one map probe; `len` is O(1) (`split_last` of the
  chunk list).

## Routing Table
- `store.rs` → `NAMES_PER_CHUNK`, `INDEX_SHARDS`, `NameChunk`, `NameChunks`, `IndexShard`,
  `IndexShards`, `shard_of`; consumed only by `../atom.rs` (`AtomTable`) and, for the two
  constants, by `../../../tests/intern_alloc.rs`.
