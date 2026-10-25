# `beam_codec` — Integration Suites (`tests/`)

## Intent
The crate's **integration** layer: one Rust binary per behavioral area of the External Term
Format codec, exercising only the **public** API (`beam_codec::{decode_term, encode_term,
decode_term_with_local_node, Encoder, Decoder, term::*, tag consts, compress, …}`) on top of
`beam_alloc::Heap` / `beam_term::AtomTable`.
The per-module unit suites live in `../src/**` (`#[cfg(test)] mod tests`, with the `pub(crate)`
helpers `src/encode/tests.rs` / `src/decode/tests.rs` expose).
34 integration binaries + 2 support submodules + 1 shared image module.
The crate-level Intent / API Surface / Constraints / Known Issues are [`../CONTEXT.md`](../CONTEXT.md)'s
— do not restate, obey; the decode/encode wire-rule detail is
[`../src/decode/CONTEXT.md`](../src/decode/CONTEXT.md) / [`../src/encode/CONTEXT.md`](../src/encode/CONTEXT.md)'s.

## API Surface (local conventions every suite follows)
- Each suite is a standalone crate: no shared module across binaries, so the small helpers
  (`resolve_oracle`, `oracle_or_skip`, `run_erl`, `erl_binary`, `oracle_lines`/`parse_int_list`,
  `is_executable`, `utf8_atom`, image builders) are **copied per file** by convention.
- **Oracle resolution**: `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`
  (same as `../../../tools/differential`). With neither, an oracle-dependent test prints a **loud stderr
  SKIPPED banner** and `return`s — a skip, never a silent pass. Pure (pinned-vector / hand-built
  wire) tests always run.
- Wire images are **hand-built byte vectors** with the tag constants from `beam_codec` (never a
  re-derived literal), so a suite pins the wire format independently of the encoder.

## The inventory, by role
**Public API / re-export surface**
- `ref_wire_public.rs` — the reference wire mapping through the crate root *and* the `prelude`, so a
  missing re-export fails here: hand-written C goldens that fail on a word swap, every word
  boundary, the typed refusal past the id slot, and the decode-demotion/encode coupling (an image
  built with the packing decodes to an internal ref with that id and re-encodes to the same numbers).
**Property / fuzz**
- `property_roundtrip.rs` + `property_oracle.rs` over `property_support/` (`generator.rs`,
  `compare.rs`, `render.rs`, `oracle.rs`): deterministic splitmix64 corpus, seed
  `0x5EED_F00D_1234_5678`; `encode → decode → encode` byte-identity over the producible tag set under
  `V0`/`V1`/`V2`, plus the live `{minor_version,N}` matrix.
**Hardening**
- `decode_hostile.rs` — every valid image truncated at every offset and every byte flipped, plus
  targeted length/count/identifier corruption and `COMPRESSED` attacks: all a typed `Err`, never a
  panic or over-read (small decode heap; 117-case oracle classification).
- `decode_depth.rs` — the `MAX_DECODE_DEPTH` bound on a 2 MiB thread, depth-only.
**Identity / `[safe]` seams**
- `local_node.rs` — D1/D2/D4/D6/D7 and the internal form at each boundary just below a local limit,
  plus a live `erl` shape re-check.
- `local_identity_roundtrip.rs` — the local-identity round trip: the inert-by-default decode seam
  (`decode_term_with_local_node` + `Encoder::with_local_node`), a local pid/port/ref demoting to the
  internal form, byte-exact re-encode (the pinned `NEW_PID_EXT` golden, the oracle's own
  `term_to_binary(self())`, the 3-number `NEWER_REFERENCE_EXT` re-encode of a legacy local
  `REFERENCE_EXT`), and staying **external** when no identity is installed.
- `export_gate.rs` — the `[safe]` active-export seam: a mock `ExportGate` through
  `Decoder::with_export_gate` and `decode_term_with_options_and_gate[_used]`, the 32-bit-truncated
  arity, the no-gate fallback, `NEW_FUN_EXT`/non-`safe` never asking, a `COMPRESSED` image still
  gated, plus a live `binary_to_term/2 [safe]` oracle cross-check.
- `export_arity_oracle.rs` — the `EXPORT_EXT` arity truncation against a live oracle.
**Deterministic attribution**
- `alloc_count.rs` over `attrib_support/` (a thread-local counting `#[global_allocator]`) pins the
  **exact** allocations for the `copy_bound` bench payload: **1** per encode, **13** fresh / **3**
  warm per decode, image 101 bytes. Counts, never timings.
**Per-tag suites**
- `tag_coverage.rs` — a 41-row per-tag coverage table (round-trip + decode-only rows incl. the
  `ATOM_INTERNAL_REF2`/`_REF3` seeded rows) with live oracle byte cross-checks, plus a 66-row
  malformed-input → **exact** `BeamError` table (65 `BadArg` + the one `SystemLimit` depth row);
  every `Tag::covered()` tag decodes, every uncovered one is a typed `BadArg`. Shares
  `tag_images/` with `tag_space_audit`.
- `tag_space_audit.rs` — the **exhaustive tag-space audit**: a dense, sorted, exactly-256-row
  disposition sweep (one row per byte `0..=255`, asserted dense and sorted, each row's
  `Ok`/`BadArg`/`Unsupported`/`SystemLimit` class asserted exactly, and every `Ok` term's re-encode
  asserted canonically-idempotent), an independent no-panic sweep of `[131, byte]` for all 256
  bytes, the edge rows no other suite owns (a bignum sign byte of `255`, the `2^59`/`-2^59`
  small-range split, the `COMPRESSED` rows beyond `compressed_decode`'s), and a live `erl`
  cross-check of all 252 cross-checkable tag bytes + 9 edge images.
- `roundtrip.rs`, `wire_roundtrip.rs`, `ground_truth.rs`, `scalar_ext_decode.rs`,
  `legacy_scalar_oracle.rs`, `decode_bound_oracle.rs`, `port_ext_legacy.rs`,
  `ref_ext_zero_and_arity.rs`, `record_roundtrip.rs`, `internal_ids.rs`, `local_fun_encode.rs`,
  `refc_decode.rs`, `map_ext_decode.rs`, `map_key_order.rs`, `minor_version.rs`, `t2b_options.rs`
  (the `[deterministic]` and `[local]` forms, byte-exact vs the oracle), `atom_cache_writer.rs`.
- The `COMPRESSED` family's four roles:
  `compressed_decode.rs` (offline hand-built vectors), `compressed_oracle.rs` (the live
  **byte-identity** suite), `compressed_bytes.rs` (the oracle-free byte and boundary suite),
  `compressed_used.rs` (the `[used]` count, decode→re-encode a byte-identical fixed point against
  both our image and the oracle's).
**The live differential harness**
- `interop.rs`.

## Constraints
- Read-only deps: every suite consumes the crate's public API only; `../src/**` is never an
  edit-target for test work.
- Assertions must never be weakened silently: an assertion made obsolete by a behavior change is
  **updated deliberately** (with the reason) or removed — never deleted to make a suite green.
- Every `*.rs` file carries the OTP `// %CopyrightBegin% … // %CopyrightEnd%` Apache-2.0 block
  (byte-exact, enforced by `scripts/license-header.es`).
- Files target ~1000 lines; split by responsibility before growing past it — except where noted
  below (a legitimately long suite documents why at its top).

## Notes for Agents
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_codec
  --no-fail-fast` (all through the wrapper — `$HOME` is read-only; never `cd`).
- `compressed_oracle.rs` compares **whole `COMPRESSED` images byte for byte** — the wrapper choice
  *and* the deflate stream — because the stream is `beam_deflate`'s, the workspace's port of the
  zlib 1.3.2 the oracle links, so `term_to_binary(T, [{compressed,Level}])` here *is* the oracle's
  image. It is legitimately ~1.4 k lines: 40 whole-image comparisons, each a **distinct** C path or
  witness family (both wrapper-decision paths, the level matrix, the boundary crossings, payloads
  past 32 KiB moved through temp files, a real `.beam`) — not a split candidate.
- `compressed_bytes.rs` is the oracle-free **`COMPRESSED` byte and boundary** suite: embedded oracle
  goldens (`a*14/15/16/1024`, `ok`, `1`, `[]`, `<<>>`, `{1,2,3}`), the *derived* boundary sweep over
  `N = 0..=64` of `binary:copy(<<"a">>, N)` (the crossing recomputed from `Z <= body - 5`, observed
  at the equality case `N = 15`), the incompressible second path (`zeros(P) ++ Lcg300`, crossing
  `P = 33`), level-0-never-compresses, out-of-range levels ⇒ `BadArg`, and the decode→re-encode
  byte-identical fixed point. It must pass with no `erl` on `$PATH`.
- `tag_space_audit.rs` is the one place a *byte's* disposition is decided, so it is where a new or
  reclassified tag byte must be added.

## Routing Table
- Live oracle cross-checks: `interop.rs`, `property_oracle.rs`, `compressed_oracle.rs`,
  `compressed_used.rs`, `legacy_scalar_oracle.rs`, `scalar_ext_decode.rs`, `decode_bound_oracle.rs`,
  `export_arity_oracle.rs`, `map_key_order.rs`, `minor_version.rs`, `port_ext_legacy.rs`,
  `local_fun_encode.rs`, `local_node.rs`, `local_identity_roundtrip.rs`, `tag_space_audit.rs`,
  `export_gate.rs`.
- Pure (no-oracle) wire/layout suites: `roundtrip.rs`, `wire_roundtrip.rs`, `ground_truth.rs`,
  `map_ext_decode.rs`, `refc_decode.rs`, `ref_ext_zero_and_arity.rs`, `record_roundtrip.rs`,
  `internal_ids.rs`, `atom_cache_writer.rs`, `compressed_decode.rs`, `compressed_bytes.rs`,
  `decode_depth.rs`, `decode_hostile.rs`, `property_roundtrip.rs`, `t2b_options.rs`,
  `ref_wire_public.rs`, `alloc_count.rs`.
- `property_support/` → the generator/comparator/renderer/oracle shims `property_roundtrip.rs` and
  `property_oracle.rs` share
- `attrib_support/` → the counting global allocator `alloc_count.rs` installs
- `tag_images/` → the minimal hand-built images `tag_coverage.rs` and `tag_space_audit.rs` share
- `../CONTEXT.md` → crate-level Intent / API Surface / Constraints / Known Issues (authoritative)
- `../src/CONTEXT.md` → the source-side contract this API is written against
