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
  entries) instead of the whole bucket array. The shard array itself is the other half of that
  trade — `8 · INDEX_SHARDS` B per non-empty table — so the shard count has an interior optimum;
  the cost model below states it.

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
- `pub const INDEX_SHARDS: usize = 1024` — the shard count; **must** be a power of two (a
  `const` assertion pins it, because `shard_of` masks with `INDEX_SHARDS - 1`). Chosen by the
  boot measurement in the cost model, not by the copy granularity alone: the array it sizes grows
  with `INDEX_SHARDS`, the shard clone it avoids shrinks with it.
- `NameChunk { names: Vec<Arc<str>> }` — `with_capacity()`, `push(Arc<str>)` (keeps a full chunk's
  headroom, so a cloned chunk whose capacity is its length does not regrow per name), `len()`;
  test-only `name_arc(slot)`.
- `NameChunks { chunks: Option<Arc<Vec<Arc<NameChunk>>>> }` — `new()`, `fork()`, `len()`,
  `get(index) -> Option<&str>`, `push(Arc<str>) -> u32` (**the only mutating path**, returns the
  index it took), `reserve_names(total)`; test-only `container_ptr()`/`name_arc(index)`.
- `IndexShard` — one shard's **open-addressed, linear-probing** table: `slots: Vec<Option<Entry>>`
  (`len()` a power of two, or `0`) + `len: usize`, where `Entry { hash: u64, name: Arc<str>,
  atom: Atom }`. Empty state is `Vec::new()`, so a shard allocates nothing until its first insert
  (which allocates the one slot buffer, `MIN_CAPACITY = 8`); plain probing to the first `None` is
  correct because there is no delete path. `Clone` is required
  (`IndexShards::insert` detaches a shared shard with `Arc::make_mut`) and is a **deep** copy — one
  fresh name string per entry plus one slot buffer, i.e. the same shape the `HashMap` had, which is
  what the siblings' Θ(N) clone controls measure.
- `IndexShards` (`shards: Option<Arc<[Option<Arc<IndexShard>>; INDEX_SHARDS]>>`) — `new()`,
  `fork()`, `get(&str) -> Option<Atom>`, `insert(Arc<str>, Atom)` (**the only mutating path**),
  `entries()` (Σ shard `len`, O(`INDEX_SHARDS`), debug-only use); test-only `container_ptr()`.
- `hash_of(&str) -> u64` — FNV-1a 64 over the name bytes, then the splitmix64 finalizer.
  `shard_of_hash(u64) -> usize` — the low bits masked to the shard count; `shard_of(&str)` is the
  composition (test-only: the live paths reach it through `shard_of_hash` so they keep the hash).
  `probe_index(u64, mask) -> usize` — the **high** half of the hash masked to the shard's slot
  count (the low bits are the shard's, so every entry of one shard shares them).

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
  coordinating. The same holds for `hash_of`/`shard_of_hash`/`probe_index`, which is what lets the
  probe slot be a pure function too (a seeded hasher could not be).
- `NAMES_PER_CHUNK` and `INDEX_SHARDS` are **tuning constants, not contracts**: `NAMES_PER_CHUNK`
  moves the copy granularity, `INDEX_SHARDS` the shard granularity, and no caller may depend on
  either value (the two `pub` constants exist only for the allocation-budget arithmetic).
- No `unsafe`, no panicking path over runtime data: every accessor returns `Option`.

## Cost model (the reason the module exists)
- `fork()` — 0 allocations, O(1).
- The first append after a fork — at most one chunk (≤ `NAMES_PER_CHUNK` handles) and one shard
  (≈ `len / INDEX_SHARDS` entries) are materialized, i.e. O(size of the touched containers), never
  O(table). Each `Arc::make_mut` step is two allocations (the cloned payload + the new `Arc`
  block), so the array's `8 · INDEX_SHARDS` bytes move bytes rather than allocation *count*;
  measured at **9 allocations** for a fork-then-intern at 300 atoms and **10** at 3 000
  (`../../../tests/intern_alloc.rs`, case (f), which pins the bound as flat — budget 11).
- `get`/`len`/`insert` — **one** `hash_of` per call, serving both the shard mask and the in-shard
  probe slot, plus one probe. (A `HashMap<Arc<str>, Atom>` could not reuse it: `RandomState`'s
  SipHash is per-process seeded state, re-derived for every lookup, and it compares keys through an
  `Arc<str>` deref. This was the single hottest symbol on `copy_bound/etf_roundtrip`.) `len` is
  O(1) (`split_last` of the chunk list). The probe table grows at a 3/4 load factor (exact test
  `4·len >= 3·cap`), doubling the power-of-two slot count and re-placing entries from their
  **stored** hashes — never by re-hashing a string.
- **The shard array is the second term of the trade.** A non-empty table's `shards` block is
  `8 · INDEX_SHARDS` B (8 KB at 1024), and it is *retained* by every table whose array has diverged
  from its parent — i.e. by every loaded module's `fork` on its first intern — as well as
  transiently copied by that same first append (one `Arc::make_mut` step, two allocations at any
  size: the array's size moves bytes, not allocation count).
- **Why 1024** — the measurement that sizes it. A `beam_rt -noshell -eval 'halt().'` boot loads 99
  modules, each holding a module `fork`; the diverged `IndexShards` generations they retain are the
  dominant store cost, because a retained shard clone is one map sized `name_count / INDEX_SHARDS`.
  At 128 shards that was 7.39 MiB of the 64.48 MiB peak (4 747 shard containers + 97 shard arrays +
  170 name chunks, against the ~0.55 MiB the one shared store needs). Median peak RSS, 5
  interleaved runs each, `taskset -c 3`: 128 → 64.48 MiB, 512 → 60.16, **1024 → 59.37**, 2048 →
  59.68 (past 1024 the 8 KB→16 KB per-table array outgrows what the smaller shard clone saves);
  massif's store subtree 7.90 → 3.06 MiB, its peak heap 44.06 → 36.97 MB.
  Nothing observable moves with the count: `shard_of` is a pure function of the name bytes, so
  indices, fork/COW isolation, generations, the `snapshot`/`instance` witness and `Arc<str>`
  pointer identity are unchanged, and `system_info(atom_count)`, `maps:to_list`, `phash2`,
  `term_to_binary` and `code:all_loaded` are byte-identical to the oracle's.

## Routing Table
- `store.rs` → `NAMES_PER_CHUNK`, `INDEX_SHARDS`, `NameChunk`, `NameChunks`, `IndexShard`,
  `IndexShards`, `shard_of`; consumed only by `../atom.rs` (`AtomTable`) and, for the two
  constants, by `../../../tests/intern_alloc.rs`.
