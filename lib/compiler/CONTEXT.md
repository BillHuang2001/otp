# Erlang Compiler (`lib/compiler`)

## Intent
The `compiler` OTP application: the Erlang-to-BEAM compiler, exposed as the `compile` module and driven by `erlc`.
Not to be confused with `erts/` (the runtime that *executes* BEAM files) — this app only produces BEAM object code.
It is one of the first-built applications (`ERTS_APPLICATIONS = stdlib sasl kernel compiler` in `../Makefile`) because everything else is compiled by it.
Current version floor `COMPILER_VSN = 10.0.6` (see `vsn.mk`); release-wide mapping is `otp_versions.table` at the repo root.

## API Surface
- `src/compile.erl` — the only public entry point other apps/tools call: `file/1,2`, `noenv_file/2`, `string/1,2`, `forms/1,2`, `output_generated/1`, `options/0` (`--help`), `env_compiler_options/0`; the erlc-facing `compile/3`/`compile_asm/3`/`compile_core/3`/`compile_abstr/3`.
- `src/cerl.erl` (+ `cerl_trees`, `cerl_clauses`) — the documented Core Erlang AST API (used by `lfe` and other external tooling); `core_lib` is the undocumented Core AST helper library also relied on externally.
- `src/compiler.app.src` — application resource template (`vsn` from `%VSN%`); declares `{applications,[kernel,stdlib]}` and `{runtime_dependencies, ["stdlib-...","kernel-8.4","erts-13.0","crypto-5.1"]}`.
- Input forms accepted: Erlang source (`.erl`), abstract code (`.abstr`/forms), Core Erlang (`.core`), BEAM assembly (`.S`); see `passes_1/1` in `compile.erl`.
- Consumed indirectly by `dialyzer`, `syntax_tools`, `eunit`, `debugger`, `xmerl` (parse transforms) and the build system's `erlc`.

## Constraints
- Must build very early with only `kernel` + `stdlib` at compile time — never add a compile-time dependency on a higher-level app.
- Source must stay `-Werror` clean and fully spec'd (`ERL_COMPILE_FLAGS` include `-Werror +inline +warn_missing_spec`); the test suites are additionally self-linted with `+clint +ssalint`.
- The pass *order* is semantic and is encoded as data in `compile.erl` — changing the pipeline means editing the pass-list builders there, not reordering modules.
- Never hand-edit generated files: `src/beam_opcodes.erl`/`.hrl` (from `genop.tab` via `erts/emulator/utils/beam_makeops`), `src/core_parse.erl` (yecc from `core_parse.yrl`), `ebin/*`, and generated `compiler.app`/`compiler.appup`.
- The public `compile` and `cerl` APIs are widely used — treat them as stable.

## Design Decisions
- Four intermediate representations, one module family per stage: Erlang abstract code → Core Erlang → BEAM SSA → BEAM assembly → BEAM binary.
- The compilation pipeline is *data*: `passes/2` selects a list of pass commands built by `standard_passes/0`, `abstr_passes/1`, `core_passes/1`, `kernel_passes/0`, `asm_passes/0`, `binary_passes/0`; `select_passes/2` expands `{iff,_}`, `{unless,_}`, `{delay,_}`, `{listing,_}`, `done` into a flat list.
- Each pass is a module exporting `module/2` (old-behavior-style `beam_*`/`sys_core_*`), and pass state is the private `#compile{}` record.
- Type information lives in the SSA layer (`beam_types` lattice + `beam_ssa_type`/`beam_call_types`), replacing the old Core-only type passes; features are version-gated via `+r29`/-feature flags (`expand_opt/2`).

## Stage 2 opt-in passes (loop opts / fusion / FBIP)
The Rust-rewrite "Stage 2" compiler workstream (root `ERTS_RUST_REWRITE_DEEPDIVE.md` §E/§H) adds three **opt-in, default-OFF** SSA passes here, each with its own CT suite.
All three are wired *inside* `src/beam_ssa_opt.erl` (NOT in `compile.erl`'s pass lists); `options/0` merely advertises their flags in `--help` (`compile.erl:3259-3261`).
- `src/beam_ssa_loop_opt.erl` — loop recognition (natural loops from back edges + dominators) + **loop-invariant code motion** (LICM) of pure non-raising tag-test BIFs (`?SAFE_BIFS`) and constructors (`?SAFE_OPS` = `put_list`/`put_tuple`) hoisted into a preheader; `?MAX_HOIST`=64. NO unrolling (deferred, documented `:79-86`), NO arithmetic hoisting (it runs before the SSA type pass), NO strength reduction / induction-variable analysis. Enabled by the `loop_opt` option, appended to `prologue_passes/1` (`beam_ssa_opt.erl:315-323`); kill via `no_ssa_opt_loop_opt` (auto-derived) or `no_ssa_opt`.
- `src/beam_ssa_fus.erl` — **map-lookup fusion**: a redundant `has_map_field` test + `get_map_element` extraction of the same key on the success edge collapse to one lookup, only when the map operand is type-proven a map. Enabled by `fusion_opt`, appended to `repeated_passes/1` (`beam_ssa_opt.erl:355-359`); kill via `no_ssa_opt_fus`. Multi-key lookup and tuple-element chain fusion are deliberately NOT implemented (no SSA op exists).
- `src/beam_ssa_fbip.erl` — **FBIP**: rewrites a *copying* `update_record` to the `inplace` hint when the source is provably fresh (a `put_tuple`/`put_tuple2`, an allocating `bs_create_bin`/`bs_init_writable`, a `call` to a local fresh-returning function, an already-`inplace` `update_record`, or an all-fresh `phi`), uniquely owned (single use, not a function argument) and positioned so the update runs at most once per allocation. Covers tuples/records (+ writable binaries as source); STILL no in-place reuse for lists/maps. Enabled by `fbip_opt`: `beam_ssa_opt:late_epilogue_passes/1` records the flag in the process dictionary (`put('$fbip_opt',…)` `:385`) and `beam_ssa_destructive_update:opt/2` invokes it after the regular alias/destructive transforms (`:154-163`).

## Dependencies
- Compile-time: `kernel`, `stdlib` only.
- Runtime only: `crypto` (encrypting/decrypting the `debug_info` chunk via `beam_lib:get_crypto_key/1` + `crypto:crypto_one_time/6`) — hence `DIA_PLT_APPS=crypto` in `Makefile`.
- Build tool: Perl (for `beam_makeops`), `sed`; yecc is bootstrapped from `bootstrap/`.
- `make test` here force-enables the `maybe_expr` feature via `ERL_ARGS` (see `Makefile`) — required by `maybe_SUITE` and others.

## Known Issues
- A node with `time`/`eprof`/`tprof` options exposes internal pass timing via the process dictionary (`compile__pass_times`) — debugging aid only.

## Notes for Agents
- To add or change an optimization: find the owning pass module and its position in the relevant `*_passes/0` builder in `compile.erl`; toggleable passes are wrapped in `{unless,no_<x>_opt,…}` and their flag handled in `expand_opt/2`.
- Core Erlang optimizations belong in the `core_passes/1` list; SSA optimizations in the `{delay,[...]}` block of `kernel_passes/0`; low-level BEAM peephole opts in `asm_passes/0`.
- `src/erl_bifs.erl` is the static BIF property table consumed by the Core and SSA optimizers.
- `priv/` is empty (only `.gitignore`); `scripts/` holds a manual smoke test and is NOT in `SUB_DIRECTORIES` (not installed).
- Adding a new compiler pass typically requires wiring it into `compile.erl`, `compiler.app.src`'s `modules` list, and the corresponding test suite.

## Test Strategy
- Common Test suites live in `test/` (no `TEST_NEEDS_RELEASE`): `make test` there builds an `Emakefile` and runs CT against the in-tree compiler.
- Central strategy is optimization-correctness: base feature suites are cloned by the Makefile into suffixed suites (`_no_opt`, `_no_ssa_opt`, `_no_copt`, `_inline`, `_cover`, `_dialyzer`, `_r29`, …) that re-run the same code under different optimizer option sets.
- Pass-specific suites (`beam_ssa_SUITE`, `beam_types_SUITE`, `beam_jump_SUITE`, `core_fold_SUITE`, `beam_validator_SUITE`, …), plus `random_code_SUITE` and `property_test/` (PropEr/EQC) for randomized compilation.
- `beam_validator` rejects malformed assembly; `beam_ssa_check` consumes `%%` directives in test data to assert SSA annotations.
- `scripts/smoke` is a manual, release-time end-to-end check (builds Elixir + Mix projects) — not part of CI/CT.

## Routing Table
- `src/` → all compiler sources: `compile` API + pass pipeline, Core Erlang (`v3_core`/`cerl*`/`sys_core_*`), BEAM SSA (`beam_core_to_ssa`/`beam_ssa*`/`beam_types`), codegen & assembly (`beam_ssa_codegen`/`beam_asm`/`beam_a`/`beam_block`/`beam_jump`/`beam_validator`/`beam_disasm`), generated `beam_opcodes`/`core_parse`
- `test/` → Common Test suites (feature suites, cloned option-set suites, pass suites, property tests, fixture data dirs)
- `doc/` → ExDoc build config (`docs.exs`), `Makefile` (via `make/doc.mk`), and `notes.md` (app release notes)
- `internal_doc/` → contributor-facing internals docs: `beam_ssa.md` (SSA invariants), `ssa_checks.md` (SSA checkers), `cerl-notes.md`
- `scripts/` → manual end-to-end smoke test (`smoke` escript + `smoke-mix.exs` + `smoke-build/` scratch dir); not installed, not in CI
- `priv/` → empty runtime-data dir (only `.gitignore`), reserved by convention
- `vsn.mk` / `Makefile` → `COMPILER_VSN`, `SUB_DIRECTORIES = src doc`, and the `test` override that enables `maybe_expr`
- `../make/` → shared OTP build makefiles (`app_targets.mk`, `otp_subdir.mk`, `doc.mk`) invoked from here (sibling — read-only, escalate writes to parent)
- `../stdlib/` → provides `erl_lint`, `erl_parse`, `erl_compile.hrl` used by `src/` (sibling — read-only from this node)
- `../../erts/emulator/utils/beam_makeops` → generates `src/beam_opcodes.*` from `genop.tab` (external to this app)

## See Also
- `../CONTEXT.md` — OTP application layout conventions; `../Makefile` — build order (`ERTS_APPLICATIONS`).
- `../dialyzer/`, `../syntax_tools/`, `../eunit/` — primary consumers of the `compile`/`cerl` APIs.
