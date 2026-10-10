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
- `stage2_bench.erl` — the driver behind `measure_stage2_compile.sh` (see "Compile-Throughput Measurement"). Not a suite; listed in `MODULES` so it is linted/installed like the other support modules.
- `test_lib:get_unique_files/1,2` enumerates non-cloned, non-LFE `*_SUITE` files (used for the "loaded/smoke" style suites).

## Compile-Throughput Measurement (Stage-2 passes)
`measure_stage2_compile.sh` (+ `stage2_bench.erl`) is a manual, committed measurement harness — not a suite and not in CI. It answers "what do the opt-in Stage-2 passes cost, and did they fire at all?":
- `lib/compiler/test/measure_stage2_compile.sh` (env: `REPS=N` default 5, `RIG_EBIN=DIR` to reuse a rig, `KEEP_SCRATCH=1`). Step 1 resolves `RIG_EBIN` via `../scripts/host_env.sh build` (read-only; writes only to `/tmp`), step 2 compiles `stage2_bench.erl` with the *host* `erlc` into a scratch `ebin`, step 3 runs `erl -noshell -pa <scratch> -pa $RIG_EBIN -s stage2_bench main -extra $REPS $ERL_TOP` (never an escript: escripts reject `-pa` and preload the host `compile`).
- `stage2_bench` compiles `lib/stdlib/src/*.erl` (96 modules; `-I lib/stdlib/include lib/kernel/include lib/compiler/src`) in ONE session, once per option set — `off`, `loop_opt+loop_arith_licm`, `fusion_opt`, `fbip_opt`, `all` — after one discarded `off` warmup, rotating the option order every repetition (so drift does not favour one set). It prints min/median/max wall-clock, the ratio against `off`, and the number of modules whose `Code` chunk differs from the `off` reference; the raw per-repetition numbers are printed too. Modules that fail to compile are counted and listed. It exits non-zero only on harness errors (wrong compiler, empty corpus), never because a pass did or did not fire.
- It refuses to run unless `code:which(compile)` is inside `RIG_EBIN` (exported by the wrapper): a forgotten `-pa` would otherwise silently benchmark the host OTP compiler, whose default pipeline knows nothing about these flags.
- The `Code` chunk is extracted by hand from the FOR1 container (4-byte chunk id, 32-bit size, 4-byte-aligned payload, after the unsized `BEAM` form type) because the host's `beam_lib:chunks/2` cannot read it (`{error,beam_lib,{unknown_chunk,...}}`).
- Interpretation: this host is shared/loaded, so only *ratios* mean anything (observed per-set spread is 10–40% within a run), always read the "changed" column first. Last measured (96-module stdlib corpus, host OTP 29.1.1, rig compiler 10.0.6, 5 and 7 repetitions): 24 modules change under `loop_opt+loop_arith_licm`, 2 under `fusion_opt`, 3 under `fbip_opt`, 27 under `all`; `all` is consistently the slowest option set (~+11–15% vs `off`), `fusion_opt` is indistinguishable from `off` at this noise level.

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
- The three opt-in Stage-2 pass suites (`beam_ssa_fus_SUITE`, `beam_ssa_loop_opt_SUITE`, `beam_ssa_fbip_SUITE`) exercise compile-option atoms that are OFF by default (`fusion_opt`, `loop_opt`, `fbip_opt`). Each is self-contained (compiles its own probe modules in-test, no `*_SUITE_data` dir) and needs a `compiler` that already contains the matching pass module (`beam_ssa_fus` / `beam_ssa_loop_opt` / `beam_ssa_fbip`).
- `ts_install_cth` (Common Test hook referenced in `suite/0`) comes from `common_test`.
- When `ts_install_cth`/`ct_run` is unavailable in the installed OTP, run a suite by invoking its callbacks directly (`init_per_suite/1` then each testcase from `all/0`/`groups/0`) from a small driver module. Select the freshly built compiler with `erl -pa <built-ebin>` *at startup*: an `escript` preloads the system `compile` before `code:add_patha/1` takes effect, so an escript driver silently tests the wrong compiler (opt-in passes then appear not to fire). Run the driver with `erl -noshell -pa <built-ebin> -pa <driver-dir> -s driver main`, and preload the built `.beam`s (native-record owner modules must be loaded first).
- `beam_debug_info_SUITE` compiles itself with `-compile([beam_debug_info])` on purpose (to disassemble a module carrying debug info).
- `beam_validator_SUITE_data/*.S` are hand-written BEAM assembly listings fed to `beam_validator` (invalid ones must be rejected); `beam_ssa_check_SUITE_data/*.erl` carry `%%` directives consumed by `beam_ssa_check` to assert SSA annotations.

## Running A Single Suite Without ct_run (verified recipe)
`ts_install_cth.beam` is NOT built into `common_test/ebin` in a plain in-tree build, so `suite/0`'s `{ct_hooks,[ts_install_cth]}` cannot be resolved and neither `ct_run` nor `ct:run_test/1` will start these suites. Drive the callbacks directly instead:
1. Build the tree (`./otp_build configure && make`) and compile the suites you need into a scratch ebin with the same lint flags the Makefile uses:
   In this container the build additionally needs `export CONFIG_SHELL="$(command -v bash)"` (the environment exports a stale, garbage-collected nix-store `CONFIG_SHELL`; without the override `erts/configure` aborts with `.../bin/bash: No such file or directory`) plus ncurses `CPPFLAGS`/`LDFLAGS` when ncurses is not on the default search path. `./otp_build configure && make -j14` then takes ~3 minutes and produces `bin/erl` (reports `otp=30`).
   `./bin/erlc -I lib/stdlib/include -I lib/compiler/test -o "$SCRATCH/ebin" +clint +clint0 +ssalint +nowarn_missing_spec_documented lib/compiler/test/<SUITE>.erl lib/compiler/test/test_lib.erl`
2. Run a driver that adds that ebin to the code path **at startup** and calls the callbacks itself (never from an escript: an escript preloads the system `compile` before `code:add_patha/1` takes effect; an `erl -noshell -pa <ebin> -eval ...` is safe). For each suite: `init_per_suite([{priv_dir,Dir}])`, then for every entry of `all/0` either call the testcase with that Config, or (for `{group,G}`) call `init_per_group(G,Config)`, run the testcases listed for `G` in `groups/0`, then `end_per_group/2`; finish with `end_per_suite/1`. `groups/0` entries may be 2- or 3-tuples (`{Name,TCs}` / `{Name,Props,TCs}`) and may contain non-testcase properties such as `parallel`.
   Trap: `code:ensure_loaded/1` returns `{module,M}` when the suite is already loaded, not `{ok,M}` — a driver that matches only `{ok,_}` dies before the first testcase, and since `halt/1` does not flush the io queue the failure can look like "no output at all". Have the driver append its report with `file:write_file/2` and exit with `erlang:halt(Status,[{flush,true}])`.
3. Wrap each testcase call in `try ... catch Class:Reason -> {Class,Reason} end`. A failing testcase shows up as `exit:{test_case_failed,Reason}` (`ct:fail/1` is just `exit({test_case_failed,Reason})`) or as an `error:{assertEqual,...}`/`erlang:error` from `assert.hrl`; both are caught, so no Common Test process is needed.
4. The scratch ebin must be a directory of its own: `erl -pa` on a directory with tens of thousands of entries (e.g. the shared `$TMPDIR`) makes the node fail to start.
This recipe drives the pass suites (`ssa_opt_budget_SUITE`, `beam_ssa_loop_opt_SUITE`, `beam_ssa_fus_SUITE`, `beam_ssa_fbip_SUITE`, `beam_ssa_SUITE`) against the in-tree compiler.

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
| `beam_ssa_fus_SUITE` | Opt-in map-lookup fusion pass (`beam_ssa_fus`, enabled only by the compile option atom `fusion_opt`): compiles probe modules under several option sets and asserts via `beam_disasm` that Fusion 1 (a redundant `has_map_field` + `get_map_element` pair on the success edge) and Fusion 2 (a multi-key `has_map_field` test chain covered by a matching `get_map_element`+`succeeded` extraction chain) reduce the map probes only when the pass fires, that a missing `is_map/1` guard, a key mismatch or a non-sole success edge stays unfused, and that all builds return the same values and exceptions |
| `beam_ssa_loop_opt_SUITE` | Opt-in loop pass (`beam_ssa_loop_opt`, enabled only by the compile option atom `loop_opt`, switched off by `no_ssa_opt_loop_opt` / `no_ssa_opt`): drives the pass directly on hand-built SSA (this pipeline point has no optimizable loop on real code) to assert strength reduction and induction-variable canonicalisation fire and that the strength reducer declines its unsafe cases, plus end-to-end tests that results and exceptions are unchanged with/without the flag on small loop kernels, that the kill switches are honoured, and that `loop_opt` is off by default |
| `beam_ssa_fbip_SUITE` | The opt-in FBIP pass (`beam_ssa_fbip`, enabled only by the compile option atom `fbip_opt`): compiles a generated probe module with and without the flag and asserts via `beam_disasm` that a provably-fresh `update_record` source gets hint `inplace` only with the flag (stays `copy` without it), that unsafe sources (function argument → `reuse`, value sent with `!` / captured by a fun / used twice → `copy`) keep exactly the same hint in both builds, that compile-time literal sources emit no `update_record` at all, and that both builds load and behave identically |
| `ssa_opt_budget_SUITE` | Opt-in per-SSA-pass budget/watchdog/eligibility-gate framework (`beam_ssa_opt_budget`, enabled by `ssa_opt_budget`/`{ssa_opt_budget,Millis}`, `ssa_opt_watchdog`/`{ssa_opt_watchdog,Millis}` or `ssa_opt_gates`; default OFF): compiles probes with `no_spawn_compiler_process` and inspects `beam_ssa_opt_budget:get_stats/0` to check that the machinery is off by default, that the gates skip a flat function and run a rich one without changing output, that an ample budget is a no-op, that a tight budget and the watchdog take effect, and that a budgeted build is behaviourally equivalent on synthetic and real modules |
| `beam_block_SUITE`, `beam_jump_SUITE`, `beam_reorder_SUITE`, `beam_except_SUITE`, `beam_bounds_SUITE`, `beam_validator_SUITE` | Low-level BEAM passes (basic-block, jump/`move` opt, reordering, exception), op-bound computation, and the BEAM validator |
| `inline_SUITE` | The inliner (`inline_SUITE_data/` = real modules to inline) |
| `compile_SUITE`, `compilation_SUITE` | Public `compile:file/1,2` API and options; compiles real/tough modules from `*_data/` |
| `beam_doc_SUITE`, `beam_debug_info_SUITE`, `beam_disasm_SUITE` | `-moduledoc`/doc attributes & generated docs; debug-info chunks; the BEAM disassembler |
| `warnings_SUITE`, `error_SUITE` | Compiler warnings and error diagnostics (incl. `warnings_as_errors`) |
| `regressions_SUITE` | Snippets that crashed or miscompiled the compiler in the past |
| `z_SUITE`, `random_code_SUITE` | Smoke test (`code:all_loaded/0` sanity) and property-based random compilation |
| `lfe_andor_SUITE.core`, `lfe_guard_SUITE.core` | LFE-derived Core Erlang suites (`+from_core`) |
| `property_test/` | PropEr/EQC property tests (`compile_prop.erl`, `beam_types_prop.erl`) |
| `measure_stage2_compile.sh` + `stage2_bench.erl` | Manual compile-throughput measurement of the opt-in Stage-2 passes on the whole `lib/stdlib/src` corpus (not a suite, not in CI) |
| Fixture data dirs (`*_SUITE_data/`) | Static inputs consumed by their matching suite: `beam_doc_`, `beam_ssa_check_`, `beam_validator_` (`.S` listings), `bif_`, `compilation_`, `compile_`, `core_fold_`, `core_` (`.core`), `error_`, `inline_`, `receive_`, `record_` |

## Optimization-Correctness Guardrails (lists / maps / records / destructive update)
- Only `beam_ssa_check_SUITE_data/*.erl` carry `%ssa%` directives (assertions consumed by `beam_ssa_check` via `{check_ssa,pre_ssa_opt|post_ssa_opt}` in `run_checks/3`); every other suite asserts on runtime values / exceptions instead.
- Destructive-update fixtures (all under `beam_ssa_check_SUITE_data/`): `appendable.erl` (appendable-bitstring flag), `private_append.erl` (tests that `beam_ssa_destructive_update` rewrites `bs_create_bin` appends to `private_append`), `no_reuse_hint.erl` (tests the `ssa_opt_no_reuse` pass), `tuple_inplace_checks.erl` + `tuple_inplace_abort0..3.erl` (`update_record` in-place vs. abort / infinite-loop guards), `alias.erl`, `alias_chain.erl`, `alias_non_convergence.erl` (test `beam_ssa_alias_opt:opt/2` annotations and the alias-analysis convergence limit; compiled with `-compile(no_ssa_opt_destructive_update)`).
- Runtime (non-`%ssa%`) destructive-append tests live in `bs_construct_SUITE:private_append/1` (its `private_append_1/1` covers GH-7121: alias analysis failing to mark fun arguments as aliased fooled the destructive-update pass).
- `compile_SUITE:annotations_pp/1` counts pretty-printed SSA annotations (`Result type` / `Unique` / `Aliased`) in a `.ssaopt` listing; its comment notes that correct placement and content are verified by `beam_ssa_check_SUITE`.
- `is_cloned/1` and `get_data_dir/1` recognise every clone suffix EXCEPT `_dialyzer_SUITE` (which has no data dir; `DIALYZER = bs_match`).

## See Also
- `../src/CONTEXT.md` — the compiler implementation the suites exercise (passes named after the suites).
- `../../common_test/` — the Common Test framework (`ts_install_cth`, `ct_property_test`); `../../../make/make_emakefile.in` + `make/app_targets.mk` — the Emakefile generation and `test` target.
