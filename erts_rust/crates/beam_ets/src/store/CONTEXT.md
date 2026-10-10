# `beam_ets::store` — Table-Owned Term Storage (the A.1 copy boundary)

## Intent
`TermStore` is the per-table, table-owned term arena that implements the A.1 "copy boundary": an inserted object is deep-copied into this arena so a `Table` never retains a pointer into a caller's or process heap.
It is a **never-relocating bump arena** over a `beam_alloc::AllocRef` (`AllocType::Ets`): it keeps a chain of owned blocks and hands out stable word pointers, so interior pointers handed out by the table never dangle across growth.
It also owns per-object **reclamation** (`free`), the checked-out disposition (`relinquish`, behind `ets:take/2`) and the wholesale release (`reset`, behind `delete_all_objects`, and `Drop`).
Parent: `../CONTEXT.md` (the `Table` seam). Contract source: `../../CONTEXT.md` §2 (frozen term layout) and §3 (error strategy).

## API Surface (frozen — consumers route here)
- `TermStore::new(alloc: AllocRef)`; `TermStore::with_default_allocator()` (registered `beam_alloc::AllocType::Ets` allocator).
- `copy(&mut self, root: Eterm) -> Result<Eterm>` — deep-copies the term graph rooted at `root` into table-owned memory; an immediate returns unchanged (by value).
  The copied subset is **every kind the oracle accepts in `ets:insert/2`**: immediates, cons cells, tuples, **flat maps**, **hash maps** (HAMT nodes and collision nodes copied node-by-node, no `AtomTable` needed), **funs** (all three kinds — the self-contained header/descriptor words verbatim plus a deep-copied environment), the pointer-free boxed leaves (float, bignum, internal ref, on-heap bitstring), **external pid/port/reference** (the raw data words, with the `node` term slot deep-copied) and **sub-bitstrings / off-heap refc binaries** (re-materialised as self-contained on-heap bitstrings — the payload bytes are gathered, so no off-heap pointer survives).
  The residual typed gaps are terms the oracle cannot produce here or that are not standalone terms at all (each ⇒ `BeamError::BadArg`, never a wrong result): a bare `BinRef` root (it is only ever reached as the `orig` of the refc pair `copy_sub_bits` reconstructs), a native record (`RECORD_SUBTAG`), the reserved boxed subtags (`0x04`/`0x3C`), a bare header word, and a malformed `ErlSubBits` (a null base, or `end` preceding `start`).
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
- `copy_term` recursion: immediates → bare header words rejected as non-terms → `is_shareable_literal` (only `tuple0`) → lists (2-word `[CAR,CDR]`, children copied first) → boxed dispatch on `BoxedTag::from_raw(header_subtag(hdr))` into `copy_boxed`.
- **Per-kind copiers, every one funnelling through `alloc_words`**: `copy_leaf` (float/bignum/ref/heap-bits verbatim, `header_arity(hdr) + 1` words), `copy_flatmap`, `copy_hashmap`, `copy_fun`, `copy_sub_bits`, `copy_external`.
- Flat-map copy reads the pair count from the **raw size word** (word 1), not the header arity: layout `[header, n, keys, v0…v(n-1)]` copied as `n+3` words with keys and values deep-copied.
- Hash-map copy: a HAMT node is `[header][arity words][slot …]` — the head kinds (`_HAMT_HEAD_ARRAY`/`_BITMAP`) carry one raw `size` word, an inner node (`_NODE_BITMAP`) none, and one slot per set occupancy bit (`map_header_val(hdr).count_ones()`); each slot (a `[Key, Value]` cons cell, a sub-node, or a collision node) is copied recursively. This reproduces the ERTS interior exactly, without the `AtomTable` that `beam_term::map::write_map` requires.
- Fun copy: `1 + desc_words(kind) + num_free` words; the header and descriptor words are self-contained (atom immediates + raw scalars), so they are reproduced verbatim, while the `num_free` environment words are deep-copied. One arm covers all three kinds (`Local`/`External`/`Imported`).
- Sub-bits copy: `sub_bits_parts` yields the window; `gather_bits(base, offset, size_bits)` collects the `[start, end)` bit range (byte-aligned fast path = plain slice copy, otherwise a bit-by-bit MSB-first re-pack) and `write_heap_bits` re-materialises it over `heap_bits_size_words(size_bits)` words — a **self-contained on-heap bitstring**, so an off-heap refc payload or the source's own heap is never aliased.
- External copy: `[header, node, next, payload]` reproduced verbatim (the tail words are a raw `u32`/`u64` payload and `next` carries the node creation — the M1 substitution); word 1 (`node`) **is** a term slot and is deep-copied whenever it is not an immediate.
- **The per-object accounting invariant holds across every kind.** A HAMT node tree, a fun environment or a re-materialised bitstring allocates through the same `alloc_words` run-recording path as a tuple or a cons cell, so `free`/`relinquish`/`reset`/`Drop` reclaim exactly each root's runs.

## Design Decisions
- **The frozen term layout** (`../../CONTEXT.md` §2) lets `copy_leaf` size every pointer-free leaf as `header_arity(hdr) + 1` words — verified against `beam_term`'s writers (`write_heap_bits`: `header = words-1`; `write_big`: `header = n_digits`; `HEADER_FLONUM = make_header(1, FLOAT_SUBTAG)`).
- **Literals are mostly deep-copied.** Only the **global empty tuple** (`beam_term::tuple::tuple0`, a `LazyLock`-leaked, never-reclaimed word) is shared (`copy.c:130-140`); a `LiteralArea` literal is deep-copied because the crate holds no handle that would keep the area alive.
- **The ERTS on-heap layout is reproduced, not re-derived through `beam_term`.** Flat maps, HAMT node blocks and fun headers are copied structurally (verbatim head words + recursive slots) so no `AtomTable` is needed and the copy stays independent of the term builders.
- **A sub-bitstring is re-materialised, never aliased.** Its data lives outside the term graph (an off-heap refc payload, or another heap), so the payload bytes are gathered and re-written as an on-heap bitstring; a null base or an inverted window is a typed `badarg` rather than a fault.
- **`free` returns 0 for an untracked root by design** — idempotent operator; `Drop`/`reset` do the wholesale release.
- **`free` is keyed by the root word.** Address reuse means a *stale* root handle can equal a live object's root; callers must drop the handle when they free (the table does, removing the entry in the same step). Documented, not enforced.
- **`relinquish` never returns words to the free lists** — the difference from `free`; the arena cannot shrink, so checked-out words are held until `reset`/`Drop`.

## Known Issues / Limits (honest)
- **The residual typed gaps are the only rejects** (each ⇒ `badarg`, never a wrong result): a bare `BinRef` root (it is only ever reached as the `orig` of the refc pair `copy_sub_bits` reconstructs, never as a standalone term), a native record (`RECORD_SUBTAG` — an OTP-29 feature this toolchain does not enable), the reserved boxed subtags (`0x04`/`0x3C`), a bare header word, and a malformed `ErlSubBits` (a null base with a non-empty window, or `end` preceding `start`).
- **`take` retention.** Repeated `ets:take/2` keeps every checked-out object's words alive until `reset`/drop (see `relinquish`); ERTS frees the table's copy at take.
- **Stale-root footgun** (see Design Decisions) is a documented caller contract, not a check.

## Test Strategy
- `tests.rs` (in-module `#[cfg(test)]`) covers every copied kind and the accounting invariant:
  - immediates by value (0 words/blocks); cons/tuple/nested-tuple deep copy (distinct storage + structural `eq`);
  - flat-map round-trip (incl. a header-arity-0 case sized from the size word) and the empty flat map;
  - **hash maps** — a self-contained, equal copy and a recursive collision-node copy;
  - **funs** — all three kinds, each with its environment deep-copied;
  - **external pid/port/ref** — the `node` slot deep-copied;
  - **sub-bitstrings** and **off-heap refc binaries** — the copy is a self-contained on-heap bitstring (`the_window_expectation_matches_a_hand_computation` cross-checks the gathered bits against a hand computation);
  - pointer-free leaves copied verbatim, and the malformed-`ErlSubBits` rejection;
  - the reclamation path over the new kinds — `hash_map_and_fun_reclamation_keeps_the_accounting_stable` proves `free`'s word accounting over a HAMT node tree and a fun environment (each root's runs reclaimed exactly, no underflow);
  - plus copy survival across source-heap clobber + drop; non-relocating block growth; `Drop`/`reset` return blocks to the allocator; `free` word-exactness + idempotence + untracked no-op; exact-run reuse (`freed_runs_are_reused_only_by_later_copies`); the block-chain-stability churn test (the real-memory reclamation evidence, not just `words()`); `relinquish` semantics; and shared-literal word accounting.
- `tests/ets_store_terms.rs` (crate-level integration) cross-checks each copyable kind against the **live oracle** through the public `Table` API: it builds each term with the crate's frozen-layout writers, inserts `{Key, Term}` into a `set` and a `duplicate_bag`, reads the *table-owned* value back, **drops the source arena and any off-heap payload**, and compares the read-back's bit-exact `make_hash2` against the oracle's `erlang:phash2(ReadBack, 1 bsl 32)`. A local (`make_fun3`) fun cannot be hashed (its inlined descriptor carries no module/`old_uniq` ⇒ the typed `FunIdentityUnavailable`), so it is cross-checked via the oracle's `fun_info(F, type)`/`arity`/`env`; a pid/port/reference prints its node as a hash, so it is cross-checked via `phash2` plus `node/1`. The oracle is required by default.
- Allocator-level proofs use a private `CarrierAllocator` (`alloc.stats().live_bytes`) so `Drop`/`reset` release is asserted against the allocator, not only internals.
- Run: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets store::` (the units) and `… -p beam_ets --test ets_store_terms` (the live-oracle cross-check).

## Routing Table
- `mod.rs` → `TermStore` (arena; `copy`/`free`/`relinquish`/`reset`/`words`/`blocks`/`memory`), `StoreBlock`/`AllocSlice`, the block allocator, the copier (`copy_term`/`copy_boxed`/`copy_leaf`/`copy_flatmap`/`copy_hashmap`/`copy_fun`/`copy_sub_bits`/`copy_external` plus the module-level `gather_bits`), and the `Send`/`Sync` impls.
- `tests.rs` → the copy-boundary + reclamation unit tests and the hand-rolled term fixtures (flat map, hash map, every fun kind, external pid/port/ref, float, bignum, internal ref, heap binary, sub-bits, refc binary).

## See Also
- `../CONTEXT.md` → the `Table` seam and the crate-level Known Issues.
- `../../CONTEXT.md` §2 (frozen term layout) and §3 (error strategy / `unsafe` confinement).
- `../../erts/emulator/beam/copy.c` → the C copy engine whose semantics this mirrors (read-only).
