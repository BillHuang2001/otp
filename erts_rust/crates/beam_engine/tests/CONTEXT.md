# `beam_engine` integration tests (`tests/`)

## Intent
Integration/behaviour tests for the `beam_engine` execution tiers, driven mostly by **committed real
`.beam` fixtures** produced by the system `erlc` (OTP 29 / erts-17.1 — the differential oracle) and run
through `Machine::run_export` / `run_label`. A minority use hand-built synthetic `Module`s (the loader
is bypassed) for opcode/operand forms the standard compiler does not emit on this toolchain.
Fixtures live in `fixtures/` (see `fixtures/README.md` for the regeneration recipe and the per-fixture
opcode tables); expected values are oracle-derived, not guessed.

Most suites load committed fixtures rather than shelling out to `erl`, so they run unconditionally.
Three invoke `erl` and skip loudly when the oracle is absent (`tests/process_info.rs`,
`tests/tailcall.rs`, `tests/support/discovery.rs`) — but `process_info.rs:460` and `tailcall.rs:238`
pass the fixture dir with **`-pa`**, and the committed `fixtures/lists.beam` stand-in shadows stdlib,
so `erl` exits 1 and their live cross-checks skip **even though `erl` is on `PATH`** (use `-pz`, as
`trace.rs:602` does correctly). The `jit-cranelift` suites (`jit_cranelift.rs:55`,
`jit_cranelift_real.rs:78`) are file-level `#![cfg(feature = "jit-cranelift")]` and the feature is
`default = []` (`../Cargo.toml:20`), which CI never enables — so they compile to zero tests in a
default/CI run.

49 top-level `.rs` test files (**630 `#[test]`** functions, of which 42 are the default-off Cranelift
ones) plus the `support/` harness and `fixtures/`. No `#[ignore]`/`#[should_panic]` anywhere.

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
- No `#[ignore]`/`#[should_panic]` anywhere. Most early `return`s are the legitimate narrow
  `skip_if_unavailable()`/`oracle`-absent guards, but some silently pass on the normal path — see the
  Known Issues below.

## Known Issues (test honesty — current state)
- **Silent skips inside sweeps (a partial regression stays green):**
  `stdlib_lists.rs:757-780` (`breadth_runs_a_large_fraction_of_the_installed_corpus`) turns ANY typed
  error into a counted `SKIP` (`:778`) and only the floors (`CALL_FLOOR=480` vs the 504-target corpus,
  `MODULE_FLOOR=171` exact) catch a collapse; `exceptions_corpus.rs:370-416` skips any raise without a
  live reason term (`:377-388`) or any other `Err` (`:413-416`), floor `MODULE_FLOOR=213` vs 215
  modules; `clause_dispatch.rs:426-450` accepts ANY `Err` (identity never pinned).
- **Live-oracle cross-checks that never run** (the `-pa` footgun above): `process_info.rs:459-476`
  and every `tailcall.rs::cross_check` call site silently no-op; `tailcall.rs:304`'s pinned
  `"unsupported_gap"` is itself wrong (the real oracle prints `{'EXIT',{unsupported_gap,…}}`) and is
  masked by the dead check. `func_info_stacktrace.rs:375-394` runs a real `erl` but asserts only the
  ORACLE's output, never the engine's — it says nothing about `beam_engine`.
- **Cannot-fail assertions:** `stdlib_lists.rs:369` and `stdlib_more.rs:801`
  (`skip_reason().is_some() == stdlib_ebin().is_none()` is true by definition);
  `opcodes.rs:1971-1990` (`float_register_operand_is_read_from_the_fr_file` — both branches `Return`
  writing nothing, so `Ok(NIL)` holds either way); `gc_arena.rs:242` (`first as usize == first_addr`
  over a never-reassigned binding) and `:359-363` (compares a test-local reproduction, not the arena);
  `engine_bench.rs:1323` (`reports.len() >= 12` over a fixed 12); `jit_bench.rs:1154-1183` anchors only
  `aggr`/`cmp` — `fib`/`kernel`/`wide` compare tiers to each other (interpreter is both subject and
  oracle); `jit_linking.rs:286-295` asserts no expected value for `add`; `stdlib_lists.rs:415-424` only
  round-trips `support` helpers.
- **`jit_real_module.rs` is tier-equivalence, not oracle-checked:** it discards the corpus expected
  value (`:284` binds `_expected`) and compares errors by `Debug` string (`:195-204`, where
  `live_reason` renders as a placeholder), so the doc's "736/736 oracle-checked" overstates.
- `exceptions.rs:436-457` (and `:207-233`/`:473-506`/`:934-964`) pin behaviours that DELIBERATELY
  diverge from the oracle (short/empty stacktraces; a body-BIF raise propagating instead of landing in
  the catch frame) — labelled as locked M1 gaps, not dishonest, but they assert known-wrong results.

## Routing Table
- `fixtures/` → committed oracle `.beam`/`.erl` sources + `README.md` (regeneration recipe + per-fixture
  opcode tables) + `*_targets.txt` oracle corpora and `*_harness.erl`.
- `support/` → cross-suite helpers: the test `ModuleHost`, `SEED_ATOMS`, the OTP discovery
  (`otp_lib_root`/`all_otp_ebins`/`all_otp_beams`) used by the stdlib sweeps.
- Sibling docs: `../CONTEXT.md`, `../INSTRUCTION_SURFACE.md`, `../TEST_NOTES.md`, `../MEASURED.md`.
