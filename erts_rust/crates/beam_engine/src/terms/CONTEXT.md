# `beam_engine::terms` — boxed-term builder / reader over the heap seam

## Intent
The engine's single raw-heap boxed-term **builder and reader** layer: it obtains raw
word blocks from the execution context (`ExecutionContext::alloc_words`) and writes/reads
the frozen term layouts through `beam_term` primitives (tuples, cons cells, floats,
flat/HAMT maps, bignums, heap binaries, funs). It is one of the crate's two
`#[allow(unsafe_code)]` homes (the other is `../predicates.rs`), split by
responsibility into `construct`/`read`/`map`/`fun`/`number` + the in-module `tests`.
No BIF/scalar arithmetic lives here.

## API Surface (re-exported at `crate::terms::*`, frozen for the engine/families/`beam_rt`)
- `construct`: `make_tuple` / `make_cons` / `make_list` (builders; empty → `tuple0`/`NIL`),
  `badmatch_reason` (`{badmatch,V}` tuple), `make_heap_binary` (on-heap binary ≤ 64 bytes).
- `read`: `tuple_arity` / `tuple_element` / `cons_head` / `cons_tail` / `list_elements` /
  `binary_bytes` (byte view; rejects a non-byte-aligned bitstring).
- `map`: `make_flatmap` (flat only, ≤ 32 pairs), `make_map` (general: flat ≤ 32 distinct keys,
  else HAMT; last-value-wins), `map_entries` (storage-order pair list), `is_hashmap`.
- `fun`: `make_fun` (local closure) + readers `is_fun`/`fun_arity`/`fun_num_free`/`fun_uniq`/
  `fun_index`/`fun_free`.
- `number`: `make_float`, `make_integer` (small or exact bignum), `float_value`.
- Every builder returns `beam_common::Result`; every failure is a typed `BeamError`
  (`BadArg` / `SystemLimit` / `unsupported`), never a panic.

## Constraints
- Term construction/inspection is **only** `beam_term` (`use beam_term::prelude::*;`); this
  module owns *raw word writes* for the layouts `beam_term` does not expose a writer for,
  but defers every layout constant it can (arityval, fun header, map/bitstring sizes).
- Allocates via `ExecutionContext::alloc_words`; the pointer is valid until the next
  `ensure_heap`/collecting allocation (C `HAlloc`/`heap_top` convention).
- Apache-2.0; ~1000-line files; `unsafe` blocks carry `// SAFETY:` rationales.

## Design Decisions
- Raw-word writes for tuples/cons cells are legitimate here: `beam_term` is a dependency
  *sink* and exposes no `write_tuple`/`write_cons` writer, only the tagging
  (`make_arityval`/`make_tuple`/`make_list`), which this module uses.
- `make_map` delegates to `beam_term::map::write_map` (the HAMT/flat layout owner);
  `make_flatmap` delegates to `beam_term::thing::write_flatmap`. Both are pure forwarding.

## Known Issues / Audit Findings
- **GC root-set contract (documented, verified):** every builder allocates through the
  **unrooted** `ExecutionContext::alloc_words` — `construct.rs:47` (`make_tuple`), `:68`
  (`make_cons`, and transitively `make_list` `:85`), `:136` (`make_heap_binary`); `map.rs:46`
  (`make_flatmap`), `:72` (`make_map`); `fun.rs:75` (`make_fun`); `number.rs:34`
  (`make_float`), `:63` (`make_integer`). Nothing here ever calls
  `alloc_words_rooted`/`Machine::alloc_rooted`, so a host with a real collector gets no
  admission point while building a term and an ordinary run never triggers a collection.
  Owner of the fix: the `terms`/family/instruction-arm call sites (Handoff #11 in
  `../../CONTEXT.md`), not this module alone.
- **Duplication with `beam_term` (map reader):** `map_entries` (`map.rs:107`) +
  `flatmap_entries` (`:124`) + `hashmap_entries` (`:155`) + `collect_hashmap_node` (`:174`) +
  `hashmap_node_shape` (`:219`) re-implement `beam_term::map::map_pairs_storage_order`
  (`beam_term/src/map.rs:471` → `collect_pairs` `:431` / `collect_slots` `:398` /
  `node_slots` `:388`). A caller could use the `beam_term` primitive via
  `From<TermError> for BeamError`. The two enumerations also emit HAMT pairs in *different*
  orders (this one is a LIFO explicit stack — reverse sibling-subtree order; `beam_term` is
  slot-order recursion). `beam_term`'s version lacks the `is_non_value` array-head-slot
  guard this one has (`map.rs:191`).
- **Duplication with `beam_term` (heap binary):** `make_heap_binary` (`construct.rs:129-148`)
  hand-rolls the `ErlHeapBits` layout and re-derives `ERL_ONHEAP_BINARY_LIMIT`/
  `HEAP_BIN_HEAD_WORDS`/`WORD_BYTES` (`mod.rs:122,125,127`), duplicating
  `beam_term::bitstring::write_binary` (`beam_term/src/bitstring.rs:209`) →
  `write_heap_bits` (`:170`). The `mod.rs` doc claim that heap binaries have "no `beam_term`
  owner" is stale. Unlike `write_heap_bits`, this builder does **not** zero the tail padding
  bytes of the last data word, so the term carries arena garbage past its declared bit size.
- **Minor forwarding-adjacent duplication:** `binary_bytes`'s heap branch (`read.rs:130-137`)
  re-computes the data pointer/`HEAP_BIN_HEAD_WORDS` offset that
  `beam_term::bitstring::bitstring_parts` (`beam_term/src/bitstring.rs:299`) already returns.

## Notes for Agents
- Everything else in this module is rigorous and forwarding: tuple/cons/list readers delegate
  to `beam_term::{tuple_size,tuple_element,car,cdr}`; `make_float`/`make_integer` delegate to
  `thing::{write_float,write_big}`; `make_fun` + fun readers delegate to `beam_term::fun`.
  `list_elements` has no `beam_term` equivalent (engine-local walk). No `unwrap`/`panic`/
  `expect`/`TODO`/`allow(dead_code)` exists on the production path (all such calls are in
  `tests.rs`).

## Routing Table
- `construct.rs` → tuple/cons/list builders, `badmatch_reason`, heap-binary builder.
- `read.rs` → tuple/list/binary readers.
- `map.rs` → flat/HAMT map builder + storage-order reader.
- `fun.rs` → local-fun builder + readers.
- `number.rs` → float/integer builders + `float_value`.
- `tests.rs` → arena round-trips, raw-word layout assertions, wrong-type/limit errors.
- `../families/bitstrings/raw.rs`, `../families/records/mod.rs` → the same allocation pattern
  for bitstring/record blocks (also unrooted; see Handoff #11).
- `../../beam_term/src/{map,bitstring,thing,fun,tuple,list}.rs` → the layout owners this module
  forwards to (and, for the two findings above, duplicates).
