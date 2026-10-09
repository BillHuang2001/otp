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
The Rust-rewrite "Stage 2" compiler workstream (root `ERTS_RUST_REWRITE_DEEPDIVE.md` §E/§H) adds three **opt-in, default-OFF** SSA passes here, each with its own CT suite, plus an opt-in per-pass budget/watchdog/effort/eligibility-gate framework.
All three are wired *inside* `src/beam_ssa_opt.erl` (NOT in `compile.erl`'s pass lists); `options/0` advertises their flags in `--help` via synthetic entries (`compile.erl:3272-3300`).
Every pass and sub-pass is gated on an option that `expand_opt/2` passes through unchanged when absent, so the default compilation path stays byte-for-byte identical (measured: 0 `Code`-chunk md5 mismatches over all 98 `lib/stdlib/src/*.erl`).
The framework is `src/beam_ssa_opt_budget.erl` (`ssa_opt_budget` / `ssa_opt_fn_budget` / `ssa_opt_watchdog` / `ssa_opt_gates`; see Known Issues).

- `src/beam_ssa_loop_opt.erl` (441-line driver; flag `loop_opt`) — the loop-optimization pass, run in `repeated_passes/1` (`beam_ssa_opt.erl:366-375`), i.e. AFTER the SSA type analysis, so the `beam_ssa_type`/`beam_bounds` annotations are available.
  Its machinery is split into `beam_ssa_loop_cfg.erl` (CFG / dominators / natural-loop recognition / preheader insertion), `beam_ssa_loop_reduce.erl` (strength reduction + integrality/non-negativity proofs) and `beam_ssa_loop_arith.erl` (arithmetic LICM).
  It performs: natural-loop recognition (back edge H->P iff H dominates P) + **LICM** of pure non-raising tag-test BIFs (`?SAFE_BIFS`) and constructors (`?SAFE_OPS` = `put_list`/`put_tuple`) into the loop preheader (`?MAX_HOIST`=64); **whole-function strength reduction** performed in place (same block/destination; never moved or duplicated): `X*2^n`→`X bsl n` for a provably-integer X, `X*2`→`X+X` otherwise (exact for every number), `X div 2^n`→`X bsr n` and `X rem 2^n`→`X band (2^n-1)` for a provably non-negative integer X; and **induction-variable canonicalisation** (a loop-carried `{Init,Step}` recurrence proves integrality/non-negativity and licences canonicalising `C+V`/`C*V` to `V+C`/`V*C`; the recurrence itself is not rewritten).
  Kill via `no_ssa_opt_loop_opt` (auto-derived) or `no_ssa_opt`.
  Eligibility gate (`beam_ssa_opt:gate/1` → `beam_ssa_loop_opt:eligible/1`, consulted only under `ssa_opt_gates`) is `has_back_edge OR has_reduce_candidate` — a cheap NECESSARY precondition that must stay a SUPERSET of the pass's real effect, because the whole-function strength reducer fires with no loop.
- `loop_arith_licm` (separate pass `ssa_opt_loop_arith_licm` in `repeated_passes/1`, implemented in `beam_ssa_loop_arith.erl`) — **arithmetic LICM**: hoists non-raising arithmetic (`?ARITH_BIFS`) into the loop preheader only when every operand is loop-invariant and its integrality (and, for `div`/`rem`/`bsl`/`bsr`, the second operand) is proven AT THE PREHEADER (a loop-dependent annotation is rejected). `loop_opt` alone never enables it. Kill via `no_ssa_opt_loop_arith_licm` or `no_ssa_opt`.
- Measured effect: the loop passes DO fire on real code — 23 of 98 `lib/stdlib/src/*.erl` change under `+loop_opt +loop_arith_licm` (incl. `timer.erl`, where `T - ?MAX_RECEIVE_AFTER` is hoisted out of the `sleep/1` receive loop), with 0 compiler crashes over the sweep.
- Negative results (documented in the `beam_ssa_loop_opt.erl` moduledoc): **loop unrolling** (`loop_unroll`) and **loop-index narrowing** (`loop_index_narrowing`) are deliberately NOT implemented and have NO flags. The only CFG cycles reaching this pass are the `receive` loops built by `beam_ssa_recv` (a tail-recursive function is still a tail `call`; `beam_ssa:is_loop_header/1` recognises a header only by `peek_message`/nonzero `wait_timeout`): a receive loop has no compile-time trip count and duplicating its body is not semantics-preserving. BEAM has a single arbitrary-precision integer type with no narrow/wrapping instruction, so there is nothing to narrow an index into (the one cheaper form, `rem 2^n`→`band`, is already the strength reducer's `rem` rule).
- `src/beam_ssa_fus.erl` — **map-lookup and constant-chain fusion** (flag `fusion_opt`), appended to `repeated_passes/1` only when the flag is present, so it can never run otherwise; kill via `no_ssa_opt_fus` (auto-derived) or `no_ssa_opt`.
  Three transforms, each fired only when its precondition is proven: **Fusion 1** collapses a redundant `has_map_field(Map,Key)` test + `get_map_element(Map,Key)` extraction on the success edge into one lookup; **Fusion 2** generalises it to a chain of `has_map_field(Map,Ki)` tests whose keys are exactly those re-extracted by a following `get_map_element`+`succeeded` chain; **Fusion 4** folds a multiplication by an integer constant whose result is immediately multiplied by a second integer constant into a single multiplication by the product.
  The multi-pair BEAM instruction is still produced by `beam_block:opt_maps/2` (no new SSA op/opcode); tuple-element (`get_tuple_element`) chain fusion stays deliberately unimplemented (no fused instruction exists).
  Unlike FBIP it needs no process-dictionary option plumbing — it is invoked directly from its own registration, not from another module.
- `src/beam_ssa_fbip.erl` — **FBIP**: rewrites a *copying* `update_record` to the `inplace` hint when the source is provably fresh (a `put_tuple`/`put_tuple2`, an allocating `bs_create_bin`/`bs_init_writable`, a `call` to a local fresh-returning function, an already-`inplace` `update_record`, or an all-fresh `phi`), uniquely owned (single use, not a function argument) and positioned so the update runs at most once per allocation. Covers tuples/records (+ writable binaries as source); STILL no in-place reuse for lists/maps — map in-place is infeasible compiler-only (no map-update instruction carries a `copy`/`reuse`/`inplace` hint and the emulator always allocates a fresh map). Enabled by `fbip_opt`: `beam_ssa_opt:late_epilogue_passes/1` records the flag in the process dictionary (`put('$fbip_opt',…)` `:385`) and `beam_ssa_destructive_update:opt/2` invokes it after the regular alias/destructive transforms (`:154-163`).

## Dependencies
- Compile-time: `kernel`, `stdlib` only.
- Runtime only: `crypto` (encrypting/decrypting the `debug_info` chunk via `beam_lib:get_crypto_key/1` + `crypto:crypto_one_time/6`) — hence `DIA_PLT_APPS=crypto` in `Makefile`.
- Build tool: Perl (for `beam_makeops`), `sed`; yecc is bootstrapped from `bootstrap/`.
- `make test` here force-enables the `maybe_expr` feature via `ERL_ARGS` (see `Makefile`) — required by `maybe_SUITE` and others.

## Known Issues
- A node with `time`/`eprof`/`tprof` options exposes internal pass timing via the process dictionary (`compile__pass_times`) — debugging aid only.
- Besides the per-pass hard caps (`?MAX_REPETITIONS`=16 in `beam_ssa_opt`/`beam_ssa_alias`, `?SS_DEPTH_LIMIT`=30 in `beam_ssa_ss`, `beam_ssa_dead`'s 64-var cap) there is an **opt-in wall-clock bound** for the SSA optimization phases: `ssa_opt_budget` / `{ssa_opt_budget,Millis}` (cumulative budget for `beam_ssa_opt`, default 5000 ms), `ssa_opt_watchdog` / `{ssa_opt_watchdog,Millis}` (upper bound on a single sub-pass, default 1000 ms), `ssa_opt_gates` (enable the eligibility gates and statistics without a bound) and `ssa_opt_fn_budget` / `{ssa_opt_fn_budget,Millis}` (override the size-derived per-function allowance for every function; `0` exhausts each function immediately, activating per-function skipping). All are OFF by default and, when off, the pass funs are not wrapped at all, so the default output is byte-for-byte unchanged. Implementing module: `src/beam_ssa_opt_budget.erl`.
- `time`/`eprof`/`tprof` only measure; the budget/watchdog is what bounds. An exhausted budget stops the fixpoint and skips the still-skippable sub-passes (always a monotone suffix — the deadline never un-expires). A watchdog timeout does NOT skip the pass: the sub-pass runs in a monitored child process and a timeout aborts the compilation with reason `{ssa_opt_watchdog_timeout,Pass}` (the `timed_out` counter is still bumped), because a timeout races the pass and would otherwise skip an arbitrary, non-deterministic subset of passes, which was measured to leave the SSA inconsistent.
- Passes that are never skipped by either bound: `ssa_opt_split_blocks`, `ssa_opt_linearize`, `ssa_opt_blockify` (SSA representation and block granularity), `ssa_opt_type_finish` (publishes the type annotations the code generator and `beam_validator` rely on), `ssa_opt_live` (the only pass that drops or strength-reduces instructions whose result has become dead — a bare `get_map_element` or an unannotated `bs_start_match` reaching codegen crashes it) and `ssa_opt_bs_ensure` (annotates the binary-match blocks; `beam_ssa_pre_codegen:bs_restores_is/5` asserts the annotation is present).
- `beam_ssa_opt_equiv_SUITE`'s benchmark verdict is UNRELIABLE: its per-kernel min-of-5 in-process timing has a ±25-35% single-sample spread (the ON/OFF ratio for a fixed kernel flips sign across runs while the code is byte-identical), so the "maplookup ~18% slower" / "aggregate ~6% slower" numbers it prints are measurement noise — the opt-in passes are performance-neutral on these kernels (fusion only removes a redundant `has_map_fields` probe: `maplookup/2` shrinks 20→16 instructions and never adds work). The suite only hard-asserts the loose `ON =< OFF*2` bound. Future improvement: warm up and use the median of many repetitions instead of min-of-5.

## Notes for Agents
- To add or change an optimization: find the owning pass module and its position in the relevant `*_passes/0` builder in `compile.erl`; toggleable passes are wrapped in `{unless,no_<x>_opt,…}` and their flag handled in `expand_opt/2`.
- The opt-in Stage 2 SSA passes are the exception: they are wired *inside* `beam_ssa_opt.erl`'s phase builders (`repeated_passes/1` / `late_epilogue_passes/1`) and are appended only when their flag is a member of `Opts`; their `no_ssa_opt_<name>` kill switch is auto-derived by `passes_1/2` from the registered pass name, and a plain helper like `loop_arith_licm` is registered under `ssa_opt_loop_arith_licm` so the derived switch is `no_ssa_opt_loop_arith_licm`.
- Core Erlang optimizations belong in the `core_passes/1` list; SSA optimizations in the `{delay,[...]}` block of `kernel_passes/0`; low-level BEAM peephole opts in `asm_passes/0`.
- `src/erl_bifs.erl` is the static BIF property table consumed by the Core and SSA optimizers.
- `priv/` is empty (only `.gitignore`); `scripts/` holds a manual smoke test plus `otp30_env.sh` (a dev helper to build/test the compiler in an otherwise unbuilt OTP-30 tree), and is NOT in `SUB_DIRECTORIES` (not installed).
- Adding a new compiler pass typically requires wiring it into `compile.erl`, `compiler.app.src`'s `modules` list, and the corresponding test suite; a new `src/*.erl` module must also be added to `src/Makefile`'s `MODULES`.

## Test Strategy
- Common Test suites live in `test/` (no `TEST_NEEDS_RELEASE`): `make test` there builds an `Emakefile` and runs CT against the in-tree compiler.
- Central strategy is optimization-correctness: base feature suites are cloned by the Makefile into suffixed suites (`_no_opt`, `_no_ssa_opt`, `_no_copt`, `_inline`, `_cover`, `_dialyzer`, `_r29`, …) that re-run the same code under different optimizer option sets.
- Pass-specific suites (`beam_ssa_SUITE`, `beam_types_SUITE`, `beam_jump_SUITE`, `core_fold_SUITE`, `beam_validator_SUITE`, …), plus `random_code_SUITE` and `property_test/` (PropEr/EQC) for randomized compilation.
- The opt-in Stage 2 passes have dedicated suites (`beam_ssa_loop_opt_SUITE`, `beam_ssa_fus_SUITE`, `beam_ssa_fbip_SUITE`, all in `test/Makefile` `MODULES`); each compiles its own probe modules and needs a `compiler` built with the matching pass module.
- `ssa_opt_budget_SUITE` covers the budget/effort/watchdog/eligibility-gate framework and the `{ssa_opt_fn_budget,Ms}` option; `beam_ssa_opt_equiv_SUITE` cross-checks behavioural equivalence of the opt-in passes ON vs OFF and benchmarks them (its benchmark verdict is unreliable — see Known Issues).
- `beam_validator` rejects malformed assembly; `beam_ssa_check` consumes `%%` directives in test data to assert SSA annotations.
- `scripts/smoke` is a manual, release-time end-to-end check (builds Elixir + Mix projects) — not part of CI/CT.

## Routing Table
- `src/` → all compiler sources: `compile` API + pass pipeline, Core Erlang (`v3_core`/`cerl*`/`sys_core_*`), BEAM SSA (`beam_core_to_ssa`/`beam_ssa*`/`beam_types`), codegen & assembly (`beam_ssa_codegen`/`beam_asm`/`beam_a`/`beam_block`/`beam_jump`/`beam_validator`/`beam_disasm`), generated `beam_opcodes`/`core_parse`; also the opt-in Stage 2 passes — `beam_ssa_loop_opt` (flag `loop_opt`; machinery in `beam_ssa_loop_cfg`/`beam_ssa_loop_reduce`/`beam_ssa_loop_arith`, incl. the `loop_arith_licm` arithmetic-LICM sub-pass), `beam_ssa_fus` (flag `fusion_opt`), `beam_ssa_fbip` (flag `fbip_opt`) — and the opt-in per-pass budget/effort/watchdog/eligibility-gate framework `beam_ssa_opt_budget` (`ssa_opt_budget`/`ssa_opt_fn_budget`/`ssa_opt_watchdog`/`ssa_opt_gates`)
- `test/` → Common Test suites (feature suites, cloned option-set suites, pass suites incl. the Stage-2 `beam_ssa_loop_opt_SUITE`/`beam_ssa_fus_SUITE`/`beam_ssa_fbip_SUITE`/`ssa_opt_budget_SUITE`/`beam_ssa_opt_equiv_SUITE`, property tests, fixture data dirs)
- `doc/` → ExDoc build config (`docs.exs`), `Makefile` (via `make/doc.mk`), and `notes.md` (app release notes)
- `internal_doc/` → contributor-facing internals docs: `beam_ssa.md` (SSA invariants), `ssa_checks.md` (SSA checkers), `cerl-notes.md`
- `scripts/` → manual end-to-end smoke test (`smoke` escript + `smoke-mix.exs` + `smoke-build/` scratch dir) plus `otp30_env.sh` (dev build/test helper for an unbuilt OTP-30 tree); not installed, not in CI
- `priv/` → empty runtime-data dir (only `.gitignore`), reserved by convention
- `vsn.mk` / `Makefile` → `COMPILER_VSN`, `SUB_DIRECTORIES = src doc`, and the `test` override that enables `maybe_expr`
- `../make/` → shared OTP build makefiles (`app_targets.mk`, `otp_subdir.mk`, `doc.mk`) invoked from here (sibling — read-only, escalate writes to parent)
- `../stdlib/` → provides `erl_lint`, `erl_parse`, `erl_compile.hrl` used by `src/` (sibling — read-only from this node)
- `../../erts/emulator/utils/beam_makeops` → generates `src/beam_opcodes.*` from `genop.tab` (external to this app)

## See Also
- `../CONTEXT.md` — OTP application layout conventions; `../Makefile` — build order (`ERTS_APPLICATIONS`).
- `../dialyzer/`, `../syntax_tools/`, `../eunit/` — primary consumers of the `compile`/`cerl` APIs.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §E/§H — source of the Stage 2 compiler mandate (loop opts → fusion → FBIP, opt-in flags, per-pass budget, bootstrap benchmark); `../../erts_rust/CONTEXT.md` §"Stage 2" routes here.
