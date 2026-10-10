# `beam_codec` source root (`crates/beam_codec/src`)

## Intent
The **implementation** node of the ETF codec. Owns the wire bytes, the tag set, the
`Encoder`/`Decoder` state machines and the term-materialization layer that forwards to
`beam_term`. The crate-level intent, the frozen wire rules, the tag coverage, the
decode/encode contracts and the C ground-truth citations live in the parent
[`../CONTEXT.md`](../CONTEXT.md) — **do not restate, obey**. This file is the routing
map for the source subtree and the notes the child nodes inherit.

## API Surface (routes — see `../CONTEXT.md` for the full frozen surface)
- Root re-exports (`lib.rs`): the `Encoder`/`Decoder` entry points, `decode_term`,
  `Tag` constants, `VERSION_MAGIC`, the `error` surface, the `term` fun accessors.
  `prelude.rs` mirrors them for `use beam_codec::prelude::*;`.
- **M1 internal-reference seam (this node).** `ref_wire.rs` (crate-internal) maps a
  60-bit internal-reference `id` ↔ the 3 wire numbers of a `REFERENCE_EXT` /
  `NEW_REFERENCE_EXT` / `NEWER_REFERENCE_EXT` image (`ref_id_to_numbers` /
  `ref_numbers_to_id`, `REF_MASK`/`REF1_NUM_MASK`/`REF_NUMBERS`); `term::ref`
  (`internal_ref_id`/`make_internal_ref`) is the safe wrapper over
  `beam_term::boxed`'s frozen M1 block. Both the decoder's local-node demotion and
  the encoder's internal-reference branch consume these (single source, no
  duplication).
- `decode_term_with_local_node(bytes, heap, atoms, node, creation)` — the
  identity-carrying `decode_term` (compressed + plain), so a `binary_to_term` image
  can demote local-node pids/ports/refs. Re-exported from `lib.rs`/`prelude.rs`.
## Constraints
- Inherited from [`../CONTEXT.md`](../CONTEXT.md) §Constraints (errors only through
  `beam_common::{Result, BeamError}`; additive + inert seams; `.rs` files carry the
  OTP `// %CopyrightBegin% … // %CopyrightEnd%` Apache-2.0 header; ~1000 lines/file).
- `#![deny(unsafe_code)]` crate-wide, with exactly **one** exception: `#[allow(unsafe_code)] mod term;`
  in `lib.rs`. `decode/` and `encode/` are safe Rust and must reach raw heap words only
  through the `term/` safe wrappers (each with a `// SAFETY:` rationale).
- The boxed interior layout of every term kind (`term/`, `ref_wire`'s identity) is
  `beam_term`'s — never redefine a layout here.

## Notes for Agents
- Oracle: the system `erl`/`erlc` on `$PATH` (skip loudly if absent).
- Build/test with the wrapper: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_codec --no-fail-fast` (`$HOME` is read-only). Never `cd`.
- Sibling `../tests/` (integration binaries) is **read-only** for this node — escalate
  test changes to `..`.

## Routing Table
- `lib.rs` → crate wiring, root re-exports, the single `#[allow(unsafe_code)] mod term;`
- `ref_wire.rs` → internal-reference `id` ↔ wire-number mapping (`REF_MASK`/`REF1_NUM_MASK`, pack/unpack)
- `tag.rs` → wire-frozen tag constants + the `Tag` enum
- `reader.rs`/`writer.rs` → bounds-checked BE cursor / growable sink (safe Rust)
- `atom_cache.rs` → `AtomCache` + the `AtomCacheWriter` hook trait
- `off_heap.rs` → `OffHeapBinaries` host trait
- `compress.rs` → zlib for `COMPRESSED` (`compress`/`decompress`/`decompress_bounded`)
- `error.rs` → `bad_arg`/`system_limit`, the `unsupported*` reason atoms
- `prelude.rs` → the downstream glob-import surface
- `decode/` → the `Decoder` state machine, the tag arms, the local-node identity seam (D2/D4/D6/D7) + the internal-form demotion, `decode_term[_with_local_node]`, `decode_depth` bound
- `encode/` → the `Encoder` state machine, the tag emitters (incl. the internal pid/port/ref branches), `encode_term*`
- `term/` → the **only** `unsafe` subtree: safe forwarding/allocation wrappers over `beam_term` (`binary`/`fun`/`record`/`ref`)
