# `beam_ets::store` — Table-Owned Term Storage (the A.1 copy boundary)

## Intent
`TermStore` is the per-table, table-owned term arena that implements the A.1 "copy boundary": an inserted object is deep-copied into this arena so a `Table` never retains a pointer into a caller's or process heap.
It is a **never-relocating bump arena** over a `beam_alloc::AllocRef` (`AllocType::Ets`): it keeps a chain of owned blocks and hands out stable word pointers, so interior pointers handed out by the table never dangle across growth.
It also owns per-object **reclamation** (`free`), the checked-out disposition (`relinquish`, behind `ets:take/2`) and the wholesale release (`reset`, behind `delete_all_objects`, and `Drop`).
Parent: `../CONTEXT.md` (the `Table` seam). Contract source: `../../CONTEXT.md` §2 (frozen term layout) and §3 (error strategy).

## API Surface (frozen — consumers route here)
- `TermStore::new(alloc: AllocRef)`; `TermStore::with_default_allocator()` (registered `beam_alloc::AllocType::Ets` allocator).
- `copy(&mut self, root: Eterm) -> Result<Eterm>` — deep-copies the term graph rooted at `root` into table-owned memory; an immediate returns unchanged (by value).
  Supported subset: immediates, cons cells, tuples, **flat maps**, and the pointer-free boxed leaves (float, positive/negative bignum, internal ref, **on-heap** bitstring).
  Every other boxed kind — funs, records, external pid/port/ref, sub-bitstrings, **off-heap binary refs** and hash maps — returns the typed `BeamError::BadArg`.
  A failed copy returns the words it had already allocated to the free lists (leaks nothing).
- `free(&mut self, root: Eterm) -> usize` — reclaims the exact word runs of a copied object to exact-size free lists.
  A root the store never copied (an immediate, a foreign term, or an already-freed/checked-out root) is **a no-op returning 0** (a legitimate, documented opt-out — not error-swallowing).
- `relinquish(&mut self, root: Eterm) -> usize` — the `ets:take/2` check-out: the object stays valid, its words are **never** handed out again, and they stop counting toward `words()`; a second call or a `free` is a no-op returning 0.
- `reset(&mut self)` — releases every block through the allocator (memory → ~0) and clears all bookkeeping; the store is immediately reusable.
- `words(&self) -> usize` (live words), `blocks(&self) -> usize`, `memory(&self) -> usize` (sum of block capacities × `size_of::<Eterm>()`; freed runs are still held until `reset`/`Drop`).
- `impl Drop` routes through `reset`.
- `TermStore: Send + Sync` via hand-written `unsafe impl`s (no interior mutability).

## Constraints
- `unsafe` is confined to `store/`, `key.rs`, `select/` and small term-reading blocks in `table/`; every `unsafe fn` has a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale.
- Terms are constructed/inspected only through `beam_term`; errors are only `beam_common::{Result, BeamError}` (the reject path is `crate::error::badarg()` ⇒ `BeamError::BadArg`).
- `root` must be a live, well-formed term graph (the same precondition `beam_term::compare::cmp` states) — an `Eterm` is a raw word, so no safe function can re-check it.

## Data model / algorithms
- `StoreBlock { base: NonNull<Eterm>, capacity, used }`; `blocks: Vec<StoreBlock>` only ever grows between `reset`s, so a block **index** is a stable name.
- `AllocSlice { block, offset, words }` identifies one contiguous run.
- `alloc_words(n)`: reuse an **exactly** `n`-word freed run (free lists are bucketed by exact word count) → else bump the last block if it has room → else map a fresh block of `max(n, BLOCK_WORDS = 64)` words (one term never spans two blocks, so its words stay contiguous).
- `free_runs: Vec<(usize, Vec<AllocSlice>)>` — exact-fit buckets; `pending` / `tracked: HashMap<Eterm, Vec<AllocSlice>>` record the runs of the copy in progress / of each live root.
- `copy_term` recursion: immediates → bare header words rejected as non-terms → `is_shareable_literal` (only `tuple0`) → lists (2-word `[CAR,CDR]`, children copied first) → boxed dispatch on `BoxedTag::from_raw(header_subtag(hdr))`.
- Flat-map copy reads the pair count from the **raw size word** (word 1), not the header arity: layout `[header, n, keys, v0…v(n-1)]` copied as `n+3` words with keys and values deep-copied.

## Design Decisions
- **The frozen term layout** (`../../CONTEXT.md` §2) lets `copy_leaf` size every pointer-free leaf as `header_arity(hdr) + 1` words — verified against `beam_term`'s writers (`write_heap_bits`: `header = words-1`; `write_big`: `header = n_digits`; `HEADER_FLONUM = make_header(1, FLOAT_SUBTAG)`).
- **Literals are mostly deep-copied.** Only the **global empty tuple** (`beam_term::tuple::tuple0`, a `LazyLock`-leaked, never-reclaimed word) is shared (`copy.c:130-140`); a `LiteralArea` literal is deep-copied because the crate holds no handle that would keep the area alive.
- **`free` returns 0 for an untracked root by design** — idempotent operator; `Drop`/`reset` do the wholesale release.
- **`free` is keyed by the root word.** Address reuse means a *stale* root handle can equal a live object's root; callers must drop the handle when they free (the table does, removing the entry in the same step). Documented, not enforced.
- **`relinquish` never returns words to the free lists** — the difference from `free`; the arena cannot shrink, so checked-out words are held until `reset`/`Drop`.

## Known Issues / Limits (honest)
- **No off-heap binary support.** A binary larger than `ERL_ONHEAP_BINARY_LIMIT` (64 bytes) is an off-heap `BinRef` ⇒ `BadArg`; a table cannot store it. A large gap vs ERTS, documented in `../CONTEXT.md` Known Issues, and not exercised by the differential battery.
- **Only the `SubBits` rejection path is unit-tested here.** The catch-all `_ => Err(badarg)` and the non-flat-map `Map` arm are evidently correct but have no direct test in `tests.rs` (fun / hash-map / external pid/port/ref / `BinRef` are untested).
- **`take` retention.** Repeated `ets:take/2` keeps every checked-out object's words alive until `reset`/drop (see `relinquish`); ERTS frees the table's copy at take.
- **Stale-root footgun** (see Design Decisions) is a documented caller contract, not a check.

## Test Strategy
- `tests.rs` (in-module `#[cfg(test)]`) covers: immediates by value (0 words/blocks); cons/tuple/nested-tuple deep copy (distinct storage + structural `eq`); flat-map round-trip (incl. a header-arity-0 case sized from the size word) and the empty flat map; pointer-free leaves copied verbatim; `SubBits ⇒ BadArg`; copy survival across source-heap clobber + drop; non-relocating block growth; `Drop`/`reset` return blocks to the allocator; `free` word-exactness + idempotence + untracked no-op; exact-run reuse (`freed_runs_are_reused_only_by_later_copies`); the block-chain-stability churn test (the real-memory reclamation evidence, not just `words()`); `relinquish` semantics; and shared-literal word accounting.
- Allocator-level proofs use a private `CarrierAllocator` (`alloc.stats().live_bytes`) so `Drop`/`reset` release is asserted against the allocator, not only internals.
- Run: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets store::`.

## Routing Table
- `mod.rs` → `TermStore` (arena; `copy`/`free`/`relinquish`/`reset`/`words`/`blocks`/`memory`), `StoreBlock`/`AllocSlice`, the block allocator, the `copy_term`/`copy_boxed`/`copy_leaf`/`copy_flatmap` copier, and the `Send`/`Sync` impls.
- `tests.rs` → the copy-boundary + reclamation unit tests and the hand-rolled term fixtures (flat map, float, bignum, internal ref, heap binary, sub-bits).

## See Also
- `../CONTEXT.md` → the `Table` seam, the deep-copy subset and the crate-level Known Issues.
- `../../CONTEXT.md` §2 (frozen term layout) and §3 (error strategy / `unsafe` confinement).
- `../../erts/emulator/beam/copy.c` → the C copy engine whose semantics this mirrors (read-only).
