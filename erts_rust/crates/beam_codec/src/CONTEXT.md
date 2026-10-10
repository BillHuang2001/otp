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
  `beam_term::boxed`'s frozen M1 block. The single place either mapping is written —
  both children must route through it, never re-derive it.

## Constraints
- Inherited from [`../CONTEXT.md`](../CONTEXT.md) §Constraints (errors only through
  `beam_common::{Result, BeamError}`; additive + inert seams; `.rs` files carry the
  OTP `// %CopyrightBegin% … // %CopyrightEnd%` Apache-2.0 header; ~1000 lines/file).
- `#![deny(unsafe_code)]` crate-wide, with exactly **one** exception: `#[allow(unsafe_code)] mod term;`
  in `lib.rs`. `decode/` and `encode/` are safe Rust and must reach raw heap words only
  through the `term/` safe wrappers (each with a `// SAFETY:` rationale).
- The boxed interior layout of every term kind (`term/`, `ref_wire`'s identity) is
  `beam_term`'s — never redefine a layout here.
- Every identity-gated change must stay **additive and inert** with no local node
  installed (the distribution path is untouched).

## Notes for Agents
- Oracle: the system `erl`/`erlc` on `$PATH` (skip loudly if absent).
- Build/test with the wrapper: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_codec --no-fail-fast` (`$HOME` is read-only). Never `cd`.
- Sibling `../tests/` (integration binaries) is **read-only** for this node — escalate
  test changes to `..`.
- Local-node identifier round-trip: a local-node pid/port/ref now demotes to this
  runtime's internal form on decode (when an identity is installed) and an internal
  ref now encodes as a 3-number `NEWER_REFERENCE_EXT` (when an identity is installed);
  both are inert with no identity (the distribution path is untouched). `decode_term`
  stays identity-free — use `decode_term_with_local_node` for `binary_to_term`.
- The runtime seam is not wired yet: `beam_bifs::binconv` builds its `Decoder` without
  `with_local_node`, and its `decode_term_bif` still calls `decode_term`, so the demotion
  is inert end-to-end until a host installs the identity.

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
- `decode/` → the `Decoder` state machine, the tag arms, the local-node identity seam (D2/D4/D6/D7) and the local-node pid/port/ref **demotion** to the internal form, `decode_term[_with_local_node]`, the `MAX_DECODE_DEPTH` bound
- `encode/` → the `Encoder` state machine, the tag emitters (incl. the internal pid/port/**ref** branches), `encode_term*`
- `term/` → the **only** `unsafe` subtree: safe forwarding/allocation wrappers over `beam_term` (`binary`/`fun`/`record`/`ref`)
