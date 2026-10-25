# `beam_loader/tests/` — Fixtures, Oracle Cross-Checks, Measurement

## Intent
The crate's integration suite: the committed `.beam` fixtures, the malformed-image rejection suites, the whole-corpus load/census sweeps against the installed OTP, the live-oracle cross-checks, and the one committed performance measurement.
All of it is black-box through the public `load`/`load_file` API (plus `container::parse` where a byte-level view is the point).

## Test Strategy (rules that keep the suite honest)
- **Ground truth is derived, never guessed.** Expected IR comes from `beam_disasm:file(...)` plus a byte decode; if a fixture byte changes, re-derive the expected IR.
- **Fixture caveat:** the *installed* OTP-29 `beam_disasm` cannot decode every OTP-30 image — it aborts on `bootstrap/lib/compiler/ebin/v3_core.beam` with `{cannot_disasm_instr,{test_heap,2,{case_clause,3}}}` (a `z,3` allocation kind it does not model; kind 3 = native records appears 324× in that module's lists). For `bootstrap/` ground truth use this loader or OTP-30's own disassembler.
- **Every image is compiled `+deterministic` from a neutral `$TMPDIR` scratch dir**, so it is path-free (no `source`/`options` keys in `CInf`, no abstract code in `Dbgi`).
- **Loud skip, never a silent pass:** a test needing `erl`/`erlc` prints why it skipped when the toolchain is absent.
- **Every rejection test sits beside an unmutated control image** that loads, so a rejection cannot pass vacuously.

## Fixtures (`tests/fixtures/`) — committed `.erl` sources + oracle `.beam`
`tests/fixtures/README.md` documents regeneration and the full wire summary; read it before adding one.
- `slice.beam` (712 B) — vertical-slice module (`add/2`, `fib/1`, `pick/1`, `module_info/0,1`), the minimal generic opcode subset.
- `coverage.beam` — tuples, lists, funs (`make_fun3` + `call_fun2`, populating `FunT`), `case`, `try … catch`, floats, maps, bitstrings, the `z,3` need form, `LitT` literals.
- `records_fixture.beam` — the `Recs` fixture: four native records (`#user` `i,-1`; `#empty`; `#mixed` exercising *every* default encoding — `i,0`, the `u,0` absent sentinel, an atom, tuple/list `LitT` literals, a wide-`i` bignum; `#shared` with `-export_record([shared])`). Named `records_fixture`, **not** `records` (a stdlib `records` collision makes the oracle's `records:get_definition/2` loop forever).
- `nr_def.beam`/`nr_use.beam` — the OTP-29 native-record pair, emitting 186-191.
- `linecov.beam` (`+line_coverage`) → 183; `dbgline.beam` (`+beam_debug_info`) → 184; `bstail.beam` (`+no_ssa_opt`) → 121 (SSA opts would fold it into a `bs_match` `ensure_exactly`); `bsalloc.beam` → the `bs_create_bin` `z,3` operand.
- `literals_verbatim.beam`/`literals_compressed.beam` — the same source in the two `LitT` framings, covering every literal kind; plus `literals_oracle.escript` (Erlang-side oracle: `<otp-lib-root>` whole-corpus sweep / `--reencode <beam-file>`).

## Test files (per-file scope)
- `container.rs` — chunk directory (ids/sizes in file order), `AtU8` atoms, `ExpT`/`ImpT`, `Code` header `(0, 181, 15, 6)`, `Line`/`Type`.
- `ir.rs` — the complete per-function stream for all six `slice` functions, `func_label`/`entry_label`, `export/2` → entry label, label→index / blocks.
- `widened_ir.rs` — the widened typed IR over `coverage.beam` (typed tuple/list/fun/exception/float/map/bitstring variants, `HeapNeed` encodings, decoded `LiteralArea`, `FunDescriptor` table); also `bsalloc.beam`.
- `negative.rs` — malformed images (bad magic/form type/`format_number`, overrunning chunk size, unknown/out-of-range opcode, out-of-range atom/label, truncated varint, `z` sub-tag `0`/unknown, wrong `HeapNeed`, malformed compressed `LitT`) ⇒ a typed `BeamError`, each next to a control image.
- `negative_tables.rs` — the chunk-table/container rejections: a missing `ImpT`/`ExpT`, an `ExpT`/`LocT`/`ImpT` arity above `MAX_ARG` (and a negative one), a negative `ExpT` label, a negative `Line` value, an empty `AtU8` and the two atom-name caps, every `Type` semantic rule plus `count == 0` and a non-`ANY` first entry, and a `form_length` too small for its last chunk (with trailing bytes past the form accepted).
- `negative_ir.rs` — the code-level rejections (see `src/lower/CONTEXT.md` for the rule list), located by decoding a fixture and patching one operand in place.
- `real_modules.rs` — the broad real-OTP LOAD sweep over the generically-discovered `lib` root (`$ERTS_LIB_DIR` else `code:lib_dir()`): **34 apps / 1366 modules** here, every one must load (floors `total ≥ 1000` / `app_count ≥ 30`); plus the nested `EXPORT_EXT` fun-MFA check over `maps`/`gen_server`.
- `generic_census.rs` — the residual `Instr::Generic` census over the same `lib` root; asserts the unmodelled-opcode set equals the documented baseline (currently empty).
- `chunks.rs` — whole-corpus chunk-id classification (`chunk::classify`) + load; positive controls require all 15 corpus ids; pins key classes, cross-checks the decoded `Attr` against `module_info(attributes)` and round-trips every corpus `CInf`/`Meta`.
- `fun_name.rs` — the `FunT` function-atom oracle cross-check (scratch `fiprobe` compiled at test time): a named local fun and an anonymous lambda, located by module-unique `FunT` `index` (confirmed by `uniq`), resolve to exactly the live `erl`'s `element(2, erlang:fun_info(F, name))` (`mk` / `-anon/1-fun-0-`).
- `records.rs` — over `records_fixture.beam`: definitions reproduce the pinned `records:get_definition/2` output (declaration order, every default encoding incl. the `u,0`/`i,0` distinction and the synthesized wide-`i` bignum literal), `record_by_name`/`record` lookups, a bounded live-`erl` re-derivation, and malformed-`Recs` rejected `BadArg` beside a control image.
- `checksum.rs` — each fixture's `Module::checksum()` against a pinned `beam_lib:md5/1` hex constant (meaningful with no `erl`) and the live oracle; a 40-module generically-discovered sample cross-checked the same way; `load_file` digest == `checksum::module_md5`; `Module::default().checksum()` == `[0u8;16]`; a corrupt-`FunT` image rejected `BadArg`.
- `line_info.rs` — typed `Instr::ExecutableLine`/`Instr::DebugLine` (not `Generic`) + `Line`-table/`FuncInfo` preservation (stack traces need `current_location`); `bs_test_tail.rs` — typed `Instr::BsTestTail2` (not `Generic`), fail label inside `f/1`; `native_record.rs` — the 186-191 family over `nr_def`/`nr_use` + an `erl_eval` `is_any_native_record` positive control + a stdlib/kernel/compiler sweep asserting no family member lowers to `Generic`.
- `literals.rs` — both `LitT` framings carry identical payloads and decode to identically-encoded terms; a map-free literal round-trips **byte-exact** against the compiler's payload, a map-bearing one asserts Rust == the oracle's `term_to_binary(binary_to_term(Payload))` (`--reencode`; see the `src/` Known Issue on flat-map key order); the `nested/0` key order is pinned in both framings; per-literal-kind structural assertions; a whole-corpus sweep; and the two arena literal-tagging regression pins (`the_literal_area_words_carry_the_literal_tag`, `nested_literal_children_carry_the_literal_tag`).
- `fixtures_are_path_free.rs` — the **path-free guard**: enumerates every `tests/fixtures/*.beam` at run time (no hard-coded list) and fails on any absolute-path marker in the raw image (`/home/`, `/tmp/`, `/Source/`, `/Users/`, `/private/`, `/nix/store/`, `.genesis/`, `/genesis/`) or on a Windows drive-letter path, naming the fixture, the byte offset and the surrounding bytes; every chunk payload that is a zlib stream (ETF `COMPRESSED` `Dbgi`/`Docs`, `LitT`'s 4-byte-size framing) is inflated and scanned too. Non-vacuous: the directory must yield the committed images, and unit tests pin that the scanner detects a synthetic `/tmp/x.erl` and `C:\Users\…` while relative paths stay clean, and that a path is found inside a compressed payload.
- `load_cost.rs` — the committed, dependency-free module-load-cost measurement (200-module sample; ns/instruction, bytes/second, per-stage breakdown). **It is the only place to quote load-cost figures from** — do not carry stale numbers in prose.

## `tests/common/mod.rs` (support module, not a test target)
The generic installed-OTP discovery shared by `real_modules.rs`/`generic_census.rs` (`otp_lib_dir`/`all_otp_beams`/`lib_sweep`/`app_ebin`/`has_beams`/`skip_reason`/`rel_label`) plus raw non-normalizing ETF payload inspection (`raw_map_key_names`).
Add new shared discovery/inspection helpers here rather than copy-pasting them into a test.

## Constraints / Gate
- **GREEN required:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader` and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Always go through `erts_rust/scripts/cargo.sh` (`$HOME` is read-only in this sandbox), never `cd`, never touch `tools/` or `erts/`.
- Cross-checks against the real system ERTS as a *compatibility gate* belong to the sibling `tools/differential/` harness — these tests only probe the oracle for expectations.
- A test that needs `erl`/`erlc` must skip loudly, never silently pass.

## Notes for Agents
- Fixtures are checked-in oracle artifacts: regenerating one means re-deriving every expectation that quotes its bytes (see `tests/fixtures/README.md`).
- `Module::checksum()`-pinned hex constants keep `checksum.rs` meaningful on a machine without `erl`; keep that property when extending it.

## Routing Table
- `fixtures/` → the committed `.erl` sources + oracle `.beam` images (+ `README.md` with the regeneration recipe and wire summary).
- `common/` → the `tests/`-level support module (`common/mod.rs`).
- `../../src/CONTEXT.md` → the API those tests drive; `../../src/chunk/CONTEXT.md` / `../../src/lower/CONTEXT.md` → the rules the rejection suites pin.
