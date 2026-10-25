# `beam_codec::term` — the term-materialization layer (the crate's only `unsafe` subtree)

## Intent
The one module that writes raw heap words: it turns a decoded wire value into a **frozen-layout
term** and back into a reader, forwarding to `beam_term` (`thing`, `fun`, `bitstring`, `refc`,
`record`, `hashing`, `compare`, `tuple_size`/`tuple_element`/`car`/`cdr`).
It owns allocation *sizing* and the type/bounds guards that turn a wrong-kind term into a
`BeamError` instead of UB — nothing else.
The boxed interior layout of every term kind is `beam_term`'s; this module never redefines one.
`lib.rs` carries `#![deny(unsafe_code)]` crate-wide with exactly one exception:
`#[allow(unsafe_code)] pub mod term;`.
Ground truth: `../../../../../erts/emulator/beam/external.c` (the materialization arms) and
`erl_term.h`/`erl_map.h`/`erl_record.h` (the layouts, owned by `beam_term`).

## API Surface (crate-internal + the crate-root re-exports)
`mod.rs`
- Writers: `make_float`, `make_big`, `make_map` (**canonical**: flat ≤ 32 sorted, HAMT above —
  the ERTS constructor), `make_flatmap` (**any** pair count, see below),
  `make_external_{pid,port,ref}`.
- Readers: `float_value`, `big_parts`, `external_{pid,port,ref}_parts`, `map_pairs_storage_order`
  (ascending node-slot pre-order), the tuple/list readers; `pub use beam_term::thing::MAX_REF_IDS`.
- Constants: `BIG_ARITY_MAX`, `EXTERNAL_THING_HEAD_SIZE`, `U32_PER_WORD`.
`binary.rs`
- `make_binary`, `make_bitstring`, the off-heap `make_refc_binary` (the frozen
  `[BinRef(3)][ErlSubBits(5)]` pair), the `bitstring_*` readers, and the
  `heap_bits_size_words`/`bit_count_to_bytes`/`BITSTRING_POINTER_FLAG_MASK` sizing re-exports.
- Placement policy: at most `ERL_ONHEAP_BITS_LIMIT` (512 bits) becomes an on-heap block; above it a
  host-installed `OffHeapBinaries` owns the payload (`../decode/CONTEXT.md` §Off-heap binaries).
`fun.rs`
- `make_fun_external` (**any** arity: the true value in descriptor word 3,
  `arity & FUN_MAX_FIELD` in the 8-bit header, `FUN_DESC_WORDS_EXTERNAL = 3`),
  `make_fun_imported` (the `NEW_FUN_EXT` block, `1 + 10 + env.len()` words, **`env[k]` = the *k*-th
  wire slot**), the `fun_*_term` accessors (a miss ⇒ `BadArg`), the `beam_term::fun` re-exports, and
  `#[cfg(test)] make_fun_local` (production local-fun creation is `beam_engine::terms::make_fun`).
`record.rs`
- `is_record`, the hash-computing `make_record`, the `record_*` readers, `AM_TRUE`/`AM_FALSE`.
`ref.rs`
- `make_internal_ref(heap, id)` (allocates `INTERNAL_REF_WORDS`, calls the `unsafe`
  `write_internal_ref`) and `internal_ref_id(term)` — the crate's only internal-ref accessors,
  `// SAFETY:`-documented, **re-exported at the crate root**.
The crate root re-exports the fun accessors (`fun_arity_term`, `fun_creator_pid`, `fun_env`,
`fun_function`, `fun_index`, `fun_kind_term`, `fun_module`, `fun_num_free_term`, `fun_old_index`,
`fun_old_uniq`, `fun_true_arity_term`, `fun_uniq_bytes`, `FunKind`, `FunPid`,
`FUN_DESC_WORDS_EXTERNAL`, `FUN_MAX_FIELD`, `MAX_ARG`) plus `internal_ref_id`/`make_internal_ref`.
**Public but not part of the cross-crate contract**: dependents (`beam_bifs`, `beam_ets`,
`beam_message`, `beam_rt`) take float/bignum/map/identifier constructors from `beam_term::thing`
directly; this module keeps only the codec-internal convenience seam plus the guard constants.

## Layout ownership
The boxed interior layout of floats, bignums, flatmaps and the external pid/port/ref blocks is
`beam_term`'s (`../../../beam_term/src/thing.rs`), and this module only forwards to it; the bitstring
block (`ErlHeapBits`/`ErlSubBits` + the off-heap `BinRef` pair) is `beam_term::bitstring`/`refc`'s,
the record block `beam_term::record`'s, and the off-heap **payload** behind a `BinRef` is the
`OffHeapBinaries` host's (`beam_alloc::RefcBinary`).
The codec's own layout-relevant policy is the **wire** form of a bignum magnitude — base-256 bytes
(`big_to_bytes`/`bytes_to_big`, `big.c:3405-3490`): full eight bytes per non-leading digit, only the
leading digit's significant bytes; `SMALL_BIG_EXT` while the byte count < 256, `LARGE_BIG_EXT`
otherwise (`../encode/CONTEXT.md` §Integer width policy).
`VERSION_MAGIC` is handled by the constructor, not the term loop (`Decoder::new` requires+consumes
it, `new_body` does not; the same split on `Encoder`), so one code path serves `term_to_binary` and
the distribution payload — and it is **not** part of a distribution payload following a
distribution header.
Every returned `Eterm` belongs to the caller's heap: each heap-allocated form is written at
`heap.alloc_words`/`heap.cons`/`heap.tuple`/`make_map`'s bump pointer and returned as a tagged
pointer *into that same block* (a list is `make_list(hp)`); the codec never resets, rewinds or
reallocates the destination heap and holds no static/thread-local/`RefCell` term store, so a decoded
word stays valid exactly as long as the caller's `Heap` (`beam_alloc::Heap::reset`/`reset_to`/`Drop`
or a GC heap *replacement* invalidates it — a caller-side lifetime contract, not a codec behaviour).
Lists grow cell by cell — `LIST_EXT`/`STRING_EXT` call `heap.cons` per element (2 words per cell, no
header word, nothing contiguous): the child terms are decoded first in wire order, then the cells
are built **back-to-front**, so the head cell is the highest address and the allocation order is the
reverse of the list order.

## Records — the definition hash is the real `make_hash2`
`make_record` stores `beam_term::hashing::compute_record_definition_hash(def, atoms)` —
`make_hash2({module, name, is_exported, keys-in-declaration-order})`, what C's `records_create_4`
stores (`erl_record.c:1041-1046`) — not a placeholder.
The circularity (the hash reads only module/name/`is_exported`/declaration-ordered keys) is broken
by writing the block once with a placeholder, computing the hash from the resulting definition
term, then rewriting the **same `hp`** with `make_small(hash as i64)`; `decode_record` passes its
`atoms` through, so decoded records carry the real hash too.
The hash is **not on the wire** (round-trips stay byte-exact) but *is* observable through term
hashing (`phash2(record)`): only the **definition** hash is interning-order independent, while an
instance's `phash2` total depends on the relative interning order of the record's key atoms, so a
≥2-field record instance total is not stable across VM starts (`tests/record_roundtrip.rs` pins the
definition hash and the deterministic declaration-order tuple hash).
This crate exposes no `beam_codec::term` forwarder for definition word 2 (only
`beam_term::record::record_definition_hash`), which is not wire-observable.

## The oversized-flatmap writer (crate-local, cannot be delegated)
`make_flatmap` accepts **any** pair count; above `MAP_SMALL_MAP_LIMIT` (32) the crate-local
`write_large_flatmap` emits the same order-preserving **flat**-map image.
This cannot be delegated: `beam_term::thing::write_flatmap` asserts that limit, and
`beam_term::map::write_map` would switch to a *hash* map (a different term kind), so there is no
LCA producer for a > 32-pair flat map.
That writer is pinned word-for-word to `beam_term::thing::write_flatmap` by
`oversized_flatmap_layout_matches_beam_term`.
Pair order is preserved as-is and duplicate keys are **not** detected here (the map **decoder**
detects them via `map_size` vs the wire count).

## Constraints
- `unsafe` is confined to this module (the crate-wide `deny` plus the one `allow` on `mod term`),
  and within it to `beam_alloc` allocation calls, the `beam_term` accessors (an `Eterm`'s validity
  is a caller invariant) and the minimal type/size guards here; every `unsafe fn` carries a
  `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale.
- Errors are `beam_common::{Result, BeamError}`: a wrong-kind term, a violated bound or a
  misaligned/null host pointer is `BadArg`, never a panic.
- No layout is redefined here; a needed change to a frozen layout goes to `beam_term` and is
  reported to the crate-layer node (`../..`).

## Test Strategy
- In-module `#[cfg(test)]` suites in `mod.rs` (raw-word layout assertions over the forwarded
  `beam_term` writers, `oversized_flatmap_layout_matches_beam_term`), `binary.rs`, `fun.rs`,
  `record.rs`, `ref.rs`.
- Integration binaries that exercise this layer: `refc_decode.rs` (off-heap pair + trailing-bit
  zeroing through a mock host), `record_roundtrip.rs`, `internal_ids.rs`, `map_ext_decode.rs`,
  `tag_coverage.rs`/`tag_space_audit.rs` (every `Ok` term's re-encode asserted idempotent),
  `interop.rs` (structural comparison through these accessors).

## Routing Table
- `mod.rs` → the forwarding materialization layer (float/big/map/external-id writers and readers,
  the oversized-flatmap writer)
- `binary.rs` → the on-heap/off-heap bitstring writers + readers, the bit-size placement policy
- `fun.rs` → the `make_fun_*` writers + the `fun_*_term` accessors
- `record.rs` → `is_record`, the hash-computing `make_record`, the `record_*` readers
- `ref.rs` → `make_internal_ref`/`internal_ref_id` (safe wrappers over the internal-ref layout)
- `../decode/` → the reader that calls these writers (`../decode/CONTEXT.md`)
- `../encode/` → the reader-side users of the accessors (`../encode/CONTEXT.md`)
- `../../../beam_term/` → the frozen layouts this module forwards to (sibling within `crates/`)
- `../../tests/` → the integration binaries (sibling — read-only, escalate writes to `../..`)
