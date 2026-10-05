# Compiler — Common Test Suites

## Intent
Common Test suite directory for the `compiler` OTP application (`lib/compiler/src/`).
Validates the whole compilation pipeline — Core Erlang passes, the SSA (BEAM SSA) optimization passes, and the final BEAM assembler/validator — plus the public `compile` API, warnings/errors and documentation emission.
The central strategy: the *same* suite source is compiled and run repeatedly under different compiler-optimization option sets, so that any optimization that fails to preserve semantics shows up as a behavioural difference (see "Cloned suites").

## How Suites Are Run
- Each suite is a standard `*_SUITE.erl` exporting the usual Common Test callbacks (`all/0`, `suite/0`, `groups/0`, `init_per_suite/1`, …).
- Pragmatically every suite calls `test_lib:recompile(?MODULE)` from `init_per_suite/1`; this recompiles the suite module with its own optimization options (recovered from `module_info(compile)` and filtered by `test_lib:opt_opts/1`) when the cover server is running, then smoke-tests `beam_disasm` on the result.
- Long-running per-test work is fanned out via `test_lib:p_run/2` (parallel, bounded to ≤8 processes / scheduler count) or run sequentially on 32-bit systems (`test_lib:s_run/1`); `{comment, "N warnings"}` results are aggregated into the suite result.
- `test_lib:parallel/0` returns `[parallel]` (or `[]` on single-scheduler systems) and is used as the group property in most `groups/0`.
- Run from this directory with `make test` (builds the `Emakefile` then `erl -make`, then Common Test).
- The parent `lib/compiler/Makefile` overrides the `test` target to pass `ERL_ARGS="-enable-feature maybe_expr"`, so the `maybe_expr` language feature is enabled at runtime during tests (required by `maybe_SUITE` and others); do not run these suites without it.
- `compiler.spec` is the release test spec (`{suites,"../compiler_test",all}`); `compiler.cover` is the Cover config (instruments module `compiler`, excludes `core_scan`/`core_parse`).

## Cloned (Suffixed) Suites
`MODULES` lists the base suites. The Makefile derives *cloned* suite modules from a base suite `<S>` by `sed`-renaming its `-module(...)` (`%_X_SUITE.erl: %_SUITE.erl`), compiling each clone with a distinct option set, and running them as separate suites.
The clone lists are subsets of `MODULES` (`NO_OPT`, `INLINE`, `COVER`, `DIALYZER`, `R29`, `NO_BOOL_OPT`): cloning a feature suite under multiple option sets is what turns it into an optimization-correctness test.
Clone suffix → compile flags (via the generated `Emakefile`):
- `_no_opt_SUITE` → `+no_copt +no_bool_opt +no_share_opt +no_bsm_opt +no_fun_opt +no_ssa_opt +no_recv_opt +no_postopt` — all optimizer stages off (reference "unoptimized" semantics).
- `_post_opt_SUITE` → same as `_no_opt_SUITE` minus `+no_postopt` — higher-level opts off but the low-level post-optimization passes still run.
- `_no_copt_SUITE` → `+no_copt` only (skip Core→SSA optimizations).
- `_no_copt_ssa_SUITE` → `+no_copt +no_ssa_opt`.
- `_no_ssa_opt_SUITE` → `+no_bool_opt +no_share_opt +no_bsm_opt +no_fun_opt +no_ssa_opt +no_recv_opt`.
- `_no_bool_opt_SUITE` → `+no_bool_opt` (only `guard`, `andor`).
- `_no_module_opt_SUITE` → `+no_module_opt`; `_no_type_opt_SUITE` → `+no_type_opt`.
- `_inline_SUITE` → `+inline`; `_cover_SUITE` → `+line_coverage`; `_dialyzer_SUITE` → `+dialyzer` (only `bs_match`); `_r29_SUITE` → `+r29` (only `native_record`).
`test_lib:is_cloned/1` recognises the suffixes; `test_lib:get_data_dir/1` maps a clone's (non-existent) data dir back to the base suite's `*_SUITE_data` directory.
Generated clone `.erl` files and `Emakefile` are build artifacts — git-ignored (root `.gitignore`), never edited by hand; edit the base suite.

## Support Modules
- `test_lib.erl` / `test_lib.hrl` — shared helpers: `recompile/1`, `recompile_core/1` (from Core), `id/1`, `parallel/0`, `opt_opts/1`, `get_data_dir/1`, `smoke_disasm/1`, `p_run/2`, `highest_opcode/1`, `get_unique_files/1,2`, `assert_error_stack/3`. `test_lib.hrl` defines the `?AssertErrorStack(Error, Stack, Expr)` macro (asserts a raised error term and stacktrace shape); `test_lib:binary_part/2` and `test_lib:binary/1` exist to be overridden by BIF-override test cases.
- `erl_md5.erl` — self-contained MD5 implementation used by `misc_SUITE`'s `md5` testcase.
- `ext_records.erl` — module exporting native records (`-export_record([...])`) consumed by `native_record_SUITE`'s external-records testcase.
- `test_lib:get_unique_files/1,2` enumerates non-cloned, non-LFE `*_SUITE` files (used for the "loaded/smoke" style suites).

## Compile Flags for the Suites Themselves
`ERL_COMPILE_FLAGS += +clint +clint0 +ssalint +nowarn_missing_spec_documented` — the suites are themselves compiled with the compiler's SSA/core linters enabled (so test code exercises the linters); `+deterministic` is filtered out.

## Core-Erlang (LFE) Suites
`CORE_MODULES = lfe_andor_SUITE lfe_guard_SUITE` are compiled from `.core` (Core Erlang) sources with `+from_core`.
Their `.core` files are generated from the LFE compiler (see `lfe.readme` for the regeneration procedure; `lfe-source.patch` and `lfe-core.patch` are applied to the LFE checkout).
Dummy `.erl` stubs are generated (`ERL_DUMMY_FILES`) so the release test dir has a matching `.erl` for each `.core`.

## Property Tests (`property_test/`)
- `compile_prop.erl` — PropEr/EQC property randomly generating Erlang abstract code and compiling it (used by `random_code_SUITE`).
- `beam_types_prop.erl` — properties over the internal type lattice (`beam_types.hrl`), e.g. absorption; high repetition count.
- Both `-include_lib` `ct_property_test.hrl` and (via `-ifdef(PROPER)`) are gated on the `proper_erlang_abstract_code` add-on being present; suites using them (`random_code_SUITE`, `beam_types_SUITE`) call `ct_property_test:init_per_suite/1` and `{skip,_}` when it is unavailable.
- `random_code_SUITE` compiles random code under all compiler option combinations; the run count defaults to `?NUMTESTS = 1000` and can be overridden with the `ERL_RANDOM_CODE_NUMTESTS` environment variable.

## Constraints
- Do not edit generated clones or `Emakefile` (build artifacts); edit the base suite or the Makefile variables.
- Adding a new feature suite that must be re-run under multiple option sets means adding it to the relevant subset variable(s) in the Makefile (and to `MODULES`).
- Suite data lives in `<base>_SUITE_data/`; a clone must obtain it via `test_lib:get_data_dir/1`, not a hard-coded path.
- Keep suites feature-agnostic where possible — they are compiled with many different feature/optimization combinations.

## Notes for Agents
- `TEST_NEEDS_RELEASE` is NOT set for this app: tests run in-tree against the freshly built compiler, not an installed release.
- Tests assume a working build of `compiler` (and `stdlib`/`kernel`) is on the code path; running them from a bare checkout without `make` first will fail.
- `ts_install_cth` (Common Test hook referenced in `suite/0`) comes from `common_test`.
- `beam_debug_info_SUITE` compiles itself with `-compile([beam_debug_info])` on purpose (to disassemble a module carrying debug info).
- `beam_validator_SUITE_data/*.S` are hand-written BEAM assembly listings fed to `beam_validator` (invalid ones must be rejected); `beam_ssa_check_SUITE_data/*.erl` carry `%%` directives consumed by `beam_ssa_check` to assert SSA annotations.

## Test Strategy / Routing Table
| Suite group | What it covers |
| --- | --- |
| `guard_SUITE`, `andor_SUITE`, `apply_SUITE` | Guards, `andalso`/`orelse`/short-circuit, `apply`/`M:F(A)` semantics (also cloned as `_no_bool_opt`, `_no_opt`, `_inline`, …) |
| `record_SUITE`, `native_record_SUITE`, `match_SUITE` | Records (incl. native/`-export_record` records), pattern matching & aliased patterns |
| `map_SUITE`, `mc_SUITE` | Maps and map comprehensions |
| `lc_SUITE`, `zlc_SUITE` | List comprehensions, zip list comprehensions |
| `bs_construct_SUITE`, `bs_match_SUITE`, `bs_bincomp_SUITE`, `bs_size_expr_SUITE`, `bs_utf_SUITE`, `bs_bit_binaries_SUITE` | Bit-syntax / binaries: construction, matching, binary comprehensions, size expressions, UTF codecs, bit-level binaries |
| `trycatch_SUITE`, `receive_SUITE`, `fun_SUITE`, `float_SUITE`, `maybe_SUITE` | try/catch/after, receive (incl. ref-optimisation), funs, floats, `maybe` expressions (needs `maybe_expr` feature) |
| `bif_SUITE`, `overridden_bif_SUITE`, `misc_SUITE` | BIF handling, user-overridden BIFs, and a grab-bag of compiler edge cases |
| `core_SUITE`, `core_fold_SUITE`, `core_alias_SUITE` | Core Erlang correctness, constant folding and alias analysis |
| `beam_ssa_SUITE`, `beam_type_SUITE`, `beam_types_SUITE`, `beam_utils_SUITE`, `beam_ssa_check_SUITE` | SSA passes, type analysis/type lattice, SSA utilities, and assertions on emitted SSA annotations |
| `beam_block_SUITE`, `beam_jump_SUITE`, `beam_reorder_SUITE`, `beam_except_SUITE`, `beam_bounds_SUITE`, `beam_validator_SUITE` | Low-level BEAM passes (basic-block, jump/`move` opt, reordering, exception), op-bound computation, and the BEAM validator |
| `inline_SUITE` | The inliner (`inline_SUITE_data/` = real modules to inline) |
| `compile_SUITE`, `compilation_SUITE` | Public `compile:file/1,2` API and options; compiles real/tough modules from `*_data/` |
| `beam_doc_SUITE`, `beam_debug_info_SUITE`, `beam_disasm_SUITE` | `-moduledoc`/doc attributes & generated docs; debug-info chunks; the BEAM disassembler |
| `warnings_SUITE`, `error_SUITE` | Compiler warnings and error diagnostics (incl. `warnings_as_errors`) |
| `regressions_SUITE` | Snippets that crashed or miscompiled the compiler in the past |
| `z_SUITE`, `random_code_SUITE` | Smoke test (`code:all_loaded/0` sanity) and property-based random compilation |
| `lfe_andor_SUITE.core`, `lfe_guard_SUITE.core` | LFE-derived Core Erlang suites (`+from_core`) |
| `property_test/` | PropEr/EQC property tests (`compile_prop.erl`, `beam_types_prop.erl`) |
| Fixture data dirs (`*_SUITE_data/`) | Static inputs consumed by their matching suite: `beam_doc_`, `beam_ssa_check_`, `beam_validator_` (`.S` listings), `bif_`, `compilation_`, `compile_`, `core_fold_`, `core_` (`.core`), `error_`, `inline_`, `receive_`, `record_` |

## See Also
- `../src/CONTEXT.md` — the compiler implementation the suites exercise (passes named after the suites).
- `../../common_test/` — the Common Test framework (`ts_install_cth`, `ct_property_test`); `../../../make/make_emakefile.in` + `make/app_targets.mk` — the Emakefile generation and `test` target.
