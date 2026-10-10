# `beam_codec` — Integration Suites (`tests/`)

## Intent
The crate's **integration** layer: one Rust binary per behavioral area of the External Term
Format codec, exercising only the **public** API (`beam_codec::{decode_term, encode_term,
decode_term_with_local_node, Encoder, Decoder, term::*, tag consts, compress, …}`) on top of
`beam_alloc::Heap` / `beam_term::AtomTable`.
The per-module unit suites live in `../src/**` (`#[cfg(test)] mod tests`, with the `pub(crate)`
helpers `src/encode/tests.rs` / `src/decode/tests.rs` expose); the crate-level test strategy and
the full binary inventory are owned by `../CONTEXT.md` (§Test Strategy) — do not restate them here.

## API Surface (local conventions every suite follows)
- Each suite is a standalone crate: no shared module across binaries, so the small helpers
  (`resolve_oracle`, `oracle_or_skip`, `run_erl`, `erl_binary`, `oracle_lines`/`parse_int_list`,
  `is_executable`, `utf8_atom`, image builders) are **copied per file** by convention.
- **Oracle resolution**: `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`
  (same as `../tools/differential`). With neither, an oracle-dependent test prints a **loud stderr
  SKIPPED banner** and `return`s — a skip, never a silent pass. Pure (pinned-vector / hand-built
  wire) tests always run.
- Wire images are **hand-built byte vectors** with the tag constants from `beam_codec` (never a
  re-derived literal), so a suite pins the wire format independently of the encoder.

## Constraints
- Read-only deps: every suite consumes the crate's public API only; `../src/**` is never edit-target
  for test work.
- Assertions must never be weakened silently: an assertion made obsolete by a behavior change is
  **updated deliberately** (with the reason) or removed — never deleted to make a suite green.
- Every `*.rs` file carries the OTP `// %CopyrightBegin% … // %CopyrightEnd%` Apache-2.0 block
  (byte-exact, enforced by `scripts/license-header.es`).
- Files target ~1000 lines; split by responsibility before growing past it.

## Notes for Agents
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_codec
  --no-fail-fast` (all through the wrapper — `$HOME` is read-only; never `cd`).
- `tests/local_identity_roundtrip.rs` is the **local-identity round trip** suite: it pins the
  inert-by-default decode seam (`decode_term_with_local_node` + `Encoder::with_local_node`) — a
  local pid/port/ref demotes to the internal form, re-encodes byte-exactly (the pinned
  `NEW_PID_EXT` golden, the oracle's own `term_to_binary(self())`, the 3-number
  `NEWER_REFERENCE_EXT` re-encode of a legacy local `REFERENCE_EXT`), and stays **external** when
  no identity is installed. `tests/local_node.rs` pins the earlier identity-gated *bound checks*
  (D2/D4/D6/D7) and now also asserts the internal form at each boundary just below a local limit.

## Routing Table
- Oracle-dependent cross-checks (live `erl`) live in: `interop.rs`, `property_oracle.rs`,
  `compressed_oracle.rs`, `legacy_scalar_oracle.rs`, `scalar_ext_decode.rs`, `decode_bound_oracle.rs`,
  `export_arity_oracle.rs`, `map_key_order.rs`, `minor_version.rs`, `port_ext_legacy.rs`,
  `local_fun_encode.rs`, `local_node.rs`, `local_identity_roundtrip.rs`.
- Pure (no-oracle) wire/layout suites: `roundtrip.rs`, `wire_roundtrip.rs`, `ground_truth.rs`,
  `map_ext_decode.rs`, `refc_decode.rs`, `ref_ext_zero_and_arity.rs`, `record_roundtrip.rs`,
  `internal_ids.rs`, `atom_cache_writer.rs`, `compressed_decode.rs`, `decode_depth.rs`,
  `decode_hostile.rs`, `property_roundtrip.rs`.
- `../CONTEXT.md` → the crate-level Intent / API Surface / Tag coverage / Test Strategy (authoritative).
- `../src/CONTEXT.md` → the source-side contract this API is written against.
