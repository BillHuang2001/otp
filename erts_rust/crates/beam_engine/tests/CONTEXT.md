# `beam_engine` integration tests (`tests/`)

## Intent
Integration/behaviour tests for the `beam_engine` execution tiers, driven mostly by **committed real
`.beam` fixtures** produced by the system `erlc` (OTP 29 / erts-17.1 — the differential oracle) and run
through `Machine::run_export` / `run_label`. A minority use hand-built synthetic `Module`s (the loader
is bypassed) for opcode/operand forms the standard compiler does not emit on this toolchain.
Fixtures live in `fixtures/` (see `fixtures/README.md` for the regeneration recipe and the per-fixture
opcode tables); expected values are oracle-derived, not guessed.

These tests load committed fixtures rather than shelling out to `erl`, so they have **no environment
skip-guards** — they always run (unlike `tests/process_info.rs`, `tests/tailcall.rs` and
`tests/support/discovery.rs`, which invoke `erl` and skip loudly when it is absent).

## Constraints
- Apache-2.0 header block on every `.rs`; tests may use `#[allow(unsafe_code)]` for the frozen-layout
  bignum/bitstring/native-record readers (`beam_term::{is_big, bitstring_parts, bit_at, write_big,
  write_heap_bits, record_*}`), each with a `// SAFETY:` rationale.
- Never `cd`; run via
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.

## Fixture → suite map (the audited family suites)
- `bitstrings.beam` (`bitstrings.erl`) → `bitstrings.rs`. Emits `bs_create_bin`, `bs_match`,
  `bs_get_position`, `bs_set_position`, `bs_get_tail` (verified with `beam_disasm:file/1`). The
  module doc (lines 21-29) additionally names `bs_start_match3`/`bs_get_integer2`/`bs_get_binary2`/
  `bs_skip_bits2`/`bs_test_tail2`, which this fixture does **not** emit — those standalone IR variants
  are covered only by the synthetic modules in `bitstrings_extra_big.rs`.
- `bscreate.beam` (`bscreate.erl`) → `bitstrings_extra_create.rs` (float 16/32/64 + utf8/16/32 segments).
- `bsmatch.beam` (`bsmatch.erl`) → `bitstrings_extra_match.rs` (bs_match_string, bs_get_float2, utf reads/skips).
- `bsbig.beam` + `bssmall.beam` → `bitstrings_extra_big.rs` (>64-bit fields; + synthetic `bs_get_integer2`).
- `bsstring.beam` (`bsstring.erl`) → `bitstrings_extra_string.rs` (string constant segments).
- `bsfailseg.beam` (`bsfailseg.erl`) → `bitstrings_create_fail.rs` (`pinned_atoms/0` interns
  `badarg`/`badmatch`/`system_limit`/`error` so failures land as real `{error,Reason}` tuples).
- `mapfx.beam` (`mapfx.erl`) → `maps.rs` (get_map_elements/has_map_fields/put_map_assoc/put_map_exact;
  hashmap *source* args are hand-written HAMT blocks — no crate can build a HAMT yet).
- `records.beam` (`records.erl`) → `records.rs` fixture half (the 6 `update_record` exports + badrecord).
  The native-record **create** form (`put_record/6`) has **no oracle**: this toolchain emits no
  `put_record`, so those tests inject the instruction over the real `Recs`-chunk fixture
  (`../beam_loader/tests/fixtures/records_fixture.beam`) and are **structurally verified only**
  (`records.rs:584-600`).
- `floats.beam` (`floats.erl`) → `floats.rs` (fmove/fconv/fadd/fsub/fmul/fdiv/fnegate).

## Test Strategy / quality notes (audit)
- Every hard-coded "oracle" value sampled was re-derived against `erl` (OTP 29.1.1) and matched: e.g.
  `bscreate:f16(1.5)`→[62,0], `f16(65520.0)`→[124,0], `u8(16#10FFFF)`→[244,143,191,191];
  `bsbig:g72le(1..9)`→166599134359138271745; `bssmall:u64x`→17293822569102704640;
  `bsstring:prefix(<<7,9>>)`→[97,98,7,9], StrT=<<"abhicdabczxyQ">>; `floats:scale(1 bsl 60,1.0)`→
  1.152921504606847e18; `records:non_record(7)`→{badrecord,7}. No fabricated value found.
- Weakest error assertions (error-shape only — a wrong class/reason would still pass):
  `bitstrings.rs:201,210` (`is_err()`); `maps.rs:203-206,255-258,301-304` (`BeamError::Raise(_)`);
  `bitstrings_extra_match.rs:126-129` and `bitstrings_extra_big.rs:213-216` (`Raise(_)`).
  Strongest: `bitstrings_create_fail.rs:144-178` and `records.rs` (`assert_unsupported`,
  `assert_create_reason`) check the exact class + reason atom.
- A few structural/self-consistency tests: `records.rs:1081-1084` compares the engine's stored record
  hash to `beam_term::hashing::compute_record_definition_hash` (possibly the same function the engine
  calls); `records.rs:724-734` compares create-form defaults to the same parsed `Recs` data the engine
  reads; `bitstrings_extra_big.rs:623-670` are `read(write(x))==x` round-trips. Documented mitigations
  exist (byte-aligned `g72`/`e72` absolute pin; oracle-pinned `Recs` parse).
- No `#[ignore]`, no `#[should_panic]`, and no early-`return`/`continue` that skips on the normal path.

## Routing Table
- `fixtures/` → committed oracle `.beam`/`.erl` sources + `README.md` (regeneration recipe + per-fixture
  opcode tables) + `*_targets.txt` oracle corpora and `*_harness.erl`.
- `support/` → cross-suite helpers: the test `ModuleHost`, `SEED_ATOMS`, the OTP discovery
  (`otp_lib_root`/`all_otp_ebins`/`all_otp_beams`) used by the stdlib sweeps.
- Sibling docs: `../CONTEXT.md`, `../INSTRUCTION_SURFACE.md`, `../TEST_NOTES.md`, `../MEASURED.md`.
