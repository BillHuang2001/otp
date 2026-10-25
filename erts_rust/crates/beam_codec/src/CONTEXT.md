# `beam_codec` source root (`crates/beam_codec/src`)

## Intent
The **implementation** node of the ETF codec. Owns the wire bytes, the tag set, the
`Encoder`/`Decoder` state machines and the term-materialization layer that forwards to
`beam_term`. The crate-level intent, the frozen API surface, the tag inventory and the residual
gaps live in the parent [`../CONTEXT.md`](../CONTEXT.md) — **do not restate, obey**.
This file is the routing map for the source subtree plus the notes its children inherit; the
per-rule wire detail lives one level down in
[`decode/CONTEXT.md`](./decode/CONTEXT.md) / [`encode/CONTEXT.md`](./encode/CONTEXT.md) /
[`term/CONTEXT.md`](./term/CONTEXT.md).

## API Surface (routes — see `../CONTEXT.md` for the full frozen surface)
- Root re-exports (`lib.rs`): the `Encoder`/`Decoder` entry points, `decode_term*`/`encode_term*`,
  the `Tag` constants + `VERSION_MAGIC`, the `error` surface, the `term` fun accessors, the
  reference wire mapping. `prelude.rs` mirrors them for `use beam_codec::prelude::*;`.
- **Reference wire mapping (public API).** `ref_wire.rs` maps a 60-bit internal-reference `id` ↔
  the 3 wire numbers of a `REFERENCE_EXT` / `NEW_REFERENCE_EXT` / `NEWER_REFERENCE_EXT` image
  (`ref_id_to_numbers` / `ref_numbers_to_id`), `representable_ref_id` (the "fits the internal id
  slot" decode gate the local-node demotion consults), the checked packer
  `representable_ref_numbers` (the same bound as a typed `BeamError::SystemLimit` refusal) and
  `REF_MASK`/`REF1_NUM_MASK`/`REF_NUMBERS`. All of it is re-exported at the crate root and in
  `prelude`; a dependent crate writing or reading a wire reference uses it rather than mirroring
  the bit layout (a different pack makes the same reference come back as a different id).
  `term/ref.rs` (`internal_ref_id`/`make_internal_ref`) is the safe wrapper over
  `beam_term::boxed`'s frozen M1 block. The single place either mapping is written — both children
  and every dependent crate must route through it, never re-derive it.

## Constraints
- Inherited from [`../CONTEXT.md`](../CONTEXT.md) §Constraints (errors only through
  `beam_common::{Result, BeamError}`; additive + inert seams; `.rs` files carry the OTP
  `// %CopyrightBegin% … // %CopyrightEnd%` Apache-2.0 header; ~1000 lines/file).
- `#![deny(unsafe_code)]` crate-wide, with exactly **one** exception:
  `#[allow(unsafe_code)] pub mod term;` in `lib.rs`. `decode/` and `encode/` are safe Rust and must
  reach raw heap words only through the `term/` safe wrappers (each with a `// SAFETY:` rationale).
- The boxed interior layout of every term kind (`term/`, `ref_wire`'s identity) is `beam_term`'s —
  never redefine a layout here.
- Every identity-gated or gate-gated change must stay **additive and inert** with nothing installed
  (no local node, no `ExportGate`, no `OffHeapBinaries`) — the distribution path (`flags = 0`) and
  every pre-existing entry point stay byte-for-byte unchanged.

## Notes for Agents
- Oracle: the system `erl`/`erlc` on `$PATH` (skip loudly if absent).
- Build/test with the wrapper: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml
  -p beam_codec --no-fail-fast` (`$HOME` is read-only). Never `cd`.
- Sibling `../tests/` (integration binaries) is **read-only** for this node — escalate test changes
  to `..`.
- Local-node identifier round-trip: a local-node pid/port/ref demotes to this runtime's internal
  form on decode (when an identity is installed) and an internal ref encodes as a 3-number
  `NEWER_REFERENCE_EXT` (when an identity is installed); both are inert with no identity.
  `decode_term` stays identity-free — use `decode_term_with_local_node` for `binary_to_term`.
- Runtime wiring that exists: `beam_bifs::binconv::decode_term_bif` installs `Decoder::with_local_node`
  (via `BifContext::local_node`) plus the identity-aware compressed path, and routes
  `binary_to_term/2` through `decode_term_with_options_and_gate_used`;
  `beam_rt::context::ExportGateBridge` is the `ExportGate` implementation. Still uninstalled by any
  host: the `LOCAL_EXT` node hash (`DecodeOptions.local_hash` stays `None`) and the
  `OffHeapBinaries` payload host.

## Known Issues
- **Two source files sit over the ~1000-line target:** `decode/mod.rs` (~1485) and
  `encode/mod.rs` (~1393). Each is a single state machine — the whole `match tag` dispatch plus its
  per-tag emitters — and neither has accrued a second responsibility. A future split should move
  arms by tag family into `decode/`/`encode/` submodules, leaving `Decoder`/`Encoder` as the only
  state.
- The 60-bit internal-reference id slot (a `beam_term` layout limit that keeps a non-representable
  foreign reference external) is documented in [`decode/CONTEXT.md`](./decode/CONTEXT.md)
  §Known Issues.

## Routing Table
- `lib.rs` → crate wiring, root re-exports, the single `#[allow(unsafe_code)] pub mod term;`
- `tag.rs` → `VERSION_MAGIC` + the wire-frozen tag constants + the `Tag` enum (37 variants, 34
  covered)
- `ref_wire.rs` → the **public** internal-reference `id` ↔ wire-number mapping (pack/unpack, the
  `representable_ref_id` decode gate, the `representable_ref_numbers` checked packer); its unit
  suite pins the C goldens, the word boundaries and the typed refusal
- `reader.rs`/`writer.rs` → bounds-checked BE cursor / growable sink (safe Rust), incl.
  `Reader::u24be` and the crate-internal `patch_u32be`/`mask_last_byte` backward writes
- `atom_cache.rs` → `AtomCache` + the `AtomCacheWriter` hook trait
- `off_heap.rs` → `OffHeapBinaries` host trait
- `compress.rs` → the `COMPRESSED` plumbing, delegated to **`beam_deflate`** (the workspace's zlib
  1.3.2 port, so the streams are the oracle's bytes): `compress`/`decompress`/
  `decompress_bounded`/`decompress_bounded_used` + the level bounds; the wrapper rule lives on the
  encode side in `encode/mod.rs::compress_if_smaller`
- `error.rs` → the `unsupported*` reason atoms (`UNSUPPORTED_REASON`, `LOCAL_FUN_REASON`)
- `prelude.rs` → the downstream glob-import surface
- `decode/` → the `Decoder` state machine, the tag arms, the local-node identity seam and demotion,
  the `[safe]` flag + `ExportGate`, the `[used]` count, off-heap binaries, `decode_atom_strict`
  (the C `dec_atom`: no `NIL_EXT` marker) vs `decode_node_atom` (the C `dec_sysname`: marker
  allowed), `MAX_DECODE_DEPTH` — [`decode/CONTEXT.md`](./decode/CONTEXT.md)
- `encode/` → the `Encoder` state machine, the tag emitters (incl. the internal pid/port/**ref**
  branches), the fun/record/map/float emitters, `FunIdentity`, the flavours, `encode_term*` —
  [`encode/CONTEXT.md`](./encode/CONTEXT.md)
- `term/` → the **only** `unsafe` subtree: safe forwarding/allocation wrappers over `beam_term`
  (`binary`/`fun`/`record`/`ref`), the record definition hash, the oversized-flatmap writer —
  [`term/CONTEXT.md`](./term/CONTEXT.md)
- `../tests/` → the integration binaries (sibling — read-only, escalate writes to `..`)
