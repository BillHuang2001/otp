# Dialyzer — Source (lib/dialyzer/src)

## Intent
Core implementation of the `dialyzer` OTP application: a static **DI**screpancy **A**na**LYZ**er for **ER**lang based on *success typings* (reports only definite type errors / unreachable code, never false positives).
Analyzes either debug-compiled BEAM (`debug_info`) or Erlang source, converting both to Core Erlang, then solving a constraint/type system over the call graph.
Also ships the `typer` (TypEr) user-facing tool, which is a separate CLI built from the same type machinery.
All 29 modules are plain Erlang; there is no GUI module (the wx GUI was removed — dialyzer is CLI + library only).

## API Surface
- `dialyzer.erl` — the only module intended for other applications: `run/1`, `run_report_modules_analyzed/1`, `run_report_modules_changed_and_analyzed/1`, `plt_info/1`, `format_warning/1`, `format_warning/2`, and `plain_cl/0` (entry point used *only* by the `dialyzer` C/shell front end; never returns, calls `erlang:halt/1`).
  - `run/1` takes `dial_options()` (the documented, user-facing option list) and returns `[dial_warning()]`.
- `dialyzer.hrl` — the public/observable type vocabulary shared by every module: `dial_warning()`, `raw_warning()`, `dial_warn_tag()`, `warning_info()`, `dial_option()`, `anal_type()`, `solver()`, `warn_option()`, plus records `#options{}`, `#analysis{}`, `#plt{}`, `#plt_info{}`, `#iplt_info{}`, `#contract{}`, and the `?RET_*` exit statuses (`0`/`1`/`2`) and `?WARN_*` tags.
- `dialyzer.app.src` / `dialyzer.appup.src` — `%VSN%` substituted by `sed` at build time (see `Makefile`); appup restarts the whole application for any version change.
- `Makefile` — `RELSYSDIR=$(RELEASE_PATH)/lib/dialyzer-$(VSN)`, explicit `MODULES` list (must be updated when adding/removing a module), `HRL_FILES=dialyzer.hrl`, `APP_TARGET`/`APPUP_TARGET` generation.

### Module groups (routing map)
- Front end / CLI: `dialyzer.erl` (public API dispatcher), `dialyzer_cl.erl` (command-line driver, all analysis modes), `dialyzer_cl_parse.erl` (option parsing/validation, `start/0`,`start/1`), `dialyzer_options.erl` (`build/1` → `#options{}`; also consults the default config file (`dialyzer.config`, override with `DIALYZER_CONFIG`) and exposes `build_warnings/2`, `get_default_config_filename/0`).
- Analysis drivers: `dialyzer_analysis_callgraph.erl` (classic driver: compile + callgraph + solver orchestration, talks to the parent process), `dialyzer_incremental.erl` (incremental analysis driver, mirrors `dialyzer_cl`), `dialyzer_timing.erl` (optional timing server).
- Call graph & dependency analysis: `dialyzer_callgraph.erl` (callgraph, SCC condensation, module deps), `dialyzer_dep.erl` (core-Erlang escape/letrec/call extraction used to build the callgraph), `dialyzer_typegraph.erl` (module-level type (spec/callback) deps), `dialyzer_dot.erl` (`.dot`/`.ps` rendering of graphs).
- Code loading / Core infrastructure: `dialyzer_codeserver.erl` (ETS-backed store of Core code, contracts, callbacks, records, exported types), `dialyzer_utils.erl` (Core from src/BEAM, records & type tables, specs, labels, suppression), `dialyzer_clean_core.erl` (Core normalization required by the analyses), `cerl_prettypr.erl` (Core pretty-printer for debug/warning output).
- Solvers: `dialyzer_succ_typings.erl` (orchestrates the success-typing fixpoint over SCCs, then dataflow refinement, then warnings), `dialyzer_typesig.erl` (constraint generation + solving for an SCC; solvers `v1`/`v2`), `dialyzer_dataflow.erl` (forward dataflow/type propagation over Core and warning generation), `dialyzer_contracts.erl` (`-spec` handling and contract checking), `dialyzer_behaviours.erl` (behaviour/callback checking).
- Persistent Lookup Table: `dialyzer_plt.erl` (in-memory PLT: ETS-backed table + common ops, `new/0`, `merge_plts/1`, `plt_kind/1`), `dialyzer_cplt.erl` (classic on-disk PLT), `dialyzer_iplt.erl` (incremental on-disk PLT).
- Type infrastructure: `erl_types.erl` (the type representation and all type algebra — largest module, ~5.8 kLOC), `erl_bif_types.erl` (success typings of BIFs/ops, ~2.6 kLOC).
- Parallelism: `dialyzer_coordinator.erl` (`parallel_job/4`, job modes, dynamic label allocation), `dialyzer_worker.erl` (`launch/4`, one process per job).
- `typer` tool: `typer.erl` (CLI entry `start/0`), `typer_core.erl` (`run/1`; reentrant core used by the rebar3 plugin).

## Pipeline
1. `dialyzer:run/1` (or `plain_cl/0`) → `dialyzer_cl_parse`/`dialyzer_options:build/1` → `#options{}`.
2. `dialyzer_cl:start*` dispatches on `analysis_type`: `succ_typings` (analyze), `plt_build`, `plt_add`, `plt_remove`, `plt_check` (all routed through `plt_common/3`), or `dialyzer_incremental:start/1` for `incremental`.
3. `dialyzer_analysis_callgraph` compiles each file (`dialyzer_utils:get_core_from_{src,beam}` + `dialyzer_clean_core`), builds the callgraph (`dialyzer_callgraph` + `dialyzer_dep`) and the code server, all as parallel `compile` jobs via `dialyzer_coordinator`.
4. `dialyzer_succ_typings` condenses the callgraph to SCCs, runs `dialyzer_typesig:analyze_scc/7` per SCC (`typesig` jobs) to a fixpoint, then refines per module with `dialyzer_dataflow` (`dataflow` jobs), inserting contracts/PLT entries as it goes.
5. For `succ_typings`, a second pass collects warnings per module (`warnings` jobs: contracts, dataflow, behaviours), which `dialyzer_analysis_callgraph` filters/suppresses and reports to `dialyzer_cl`/`dialyzer_incremental`.
6. `dialyzer_cl` prints warnings via `dialyzer:format_warning/*` and returns `{Ret, Warnings}` where `Ret` is one of `?RET_NOTHING_SUSPICIOUS`/`?RET_INTERNAL_ERROR`/`?RET_DISCREPANCIES`.

## Constraints
- Compiled with `-Werror` plus `+warn_export_vars +warn_unused_import +warn_missing_spec +warn_obsolete_bool_op` (see `src/Makefile`): any new warning fails the build, so specs and variable hygiene are mandatory.
- `dialyzer.app` may depend only on `applications: [compiler, kernel, stdlib]`; `syntax_tools` (used heavily via `cerl`/`prettypr`) is declared only as a `runtime_dependencies` entry, not in `applications`.
- All app-level state is passed in `#options{}`/`#analysis{}`; there is no supervision tree and no `application` callback module (the app behaves as a collection of library-style modules).
- `beam_lib` input must carry `debug_info` (or `from = src_code`): diag "Recompile with +debug_info or analyze starting from source code" otherwise.
- PLT compatibility is keyed on `?VSN` (the dialyzer app version) *and* an implementation MD5 over key dialyzer modules (`dialyzer_cplt:compute_implementation_md5/0`); a mismatch yields `old_version` and forces a rebuild.
- `-DVSN` is injected at build time for `dialyzer_cl_parse` (prints version), `dialyzer_plt`, `dialyzer_cplt`, `dialyzer_iplt` (PLT headers/version checks) and `typer`; `%VSN%` in `dialyzer.app.src`/`dialyzer.appup.src` is substituted with `sed`.

## Design Decisions
- **Two engines for success typings**: `dialyzer_typesig` (constraint/graph-based, the path where the `v1`/`v2` solvers live; `v2` is the default, see `dialyzer_typesig:solvers/1`) produces types for an SCC, while `dialyzer_dataflow` propagates finer types module-by-module and emits the actual warnings. The `get_refined_success_typings` loop alternates between them until a fixpoint.
- **`--solver v1|v2`** is a user-visible (mostly debugging) knob forwarded verbatim from `#options.solvers` into the solver.
- **PLT classic vs incremental**: `dialyzer_cplt` keys the PLT by *file path + MD5* (not portable across machines), `dialyzer_iplt` keys by *module name* (portable) and additionally caches warnings produced per module together with the set of legal warnings; the two modules intentionally duplicate each other and are expected to diverge (`dialyzer_plt` holds the shared in-memory implementation and `plt_kind/1` distinguishes the on-disk kinds).
- Default PLT locations come from `DIALYZER_PLT`/`DIALYZER_IPLT`, else the OS user cache dir (`dialyzer_cplt:get_default_cplt_filename/0`, `dialyzer_iplt:get_default_iplt_filename/0`).
- PLT reads/writes are executed in a spawned helper process (`subproc/1` in `dialyzer_cplt`/`dialyzer_iplt`) so a corrupt/foreign PLT file cannot take down the analysis.
- Core code and ETS tables are *moved* between processes (`dialyzer_codeserver:give_away/2`) rather than copied, which is why `dialyzer_analysis_callgraph` has an explicit message protocol with its parent.
- Concurrency is fractal: `dialyzer_coordinator` runs worker processes (`dialyzer_worker`) for `compile`, `typesig`, `dataflow`, `warnings`, `contract_remote_types` and `record_remote_types` jobs, with a regulator that bounds active workers and a global core-label allocator; `typesig`/`dataflow` workers first block in `wait_for_success_typings/2`.
- Warnings are filtered twice: internal `raw_warning()` (module/MFA + location) → suppression/filtering in `dialyzer_analysis_callgraph`/`dialyzer_utils` → public `dial_warning()`.

## Known Issues
- `?WARN_UNMATCHED_RETURN` is defined as the (misspelled) atom `warn_umatched_return`; a test fixture copy under `../test/opaque_SUITE_data/src/recrec/dialyzer.hrl` uses the same atom — if the atom is ever renamed, both must change together.
- `dialyzer_dataflow.erl` (~3.8 kLOC) and `erl_types.erl` (~5.8 kLOC) are monoliths with deep internal state records; prefer targeted reads (`rg`) over full-file reads.

## Notes for Agents
- Generated artifacts, do not hand-edit: `ebin/*`, `../ebin/dialyzer.app` and `../ebin/dialyzer.appup` (from the `.src` files), `../doc/` output. `../ebin/` and `../plt/` contain only a tracked `.gitignore`.
- Adding/removing a module requires editing the `MODULES` list in `src/Makefile` (it is explicit, not globbed) and the `modules` list in `dialyzer.app.src`.
- Debug output in `dialyzer_typesig`/`dialyzer_dataflow` can emit `.dot` constraint graphs (`constraints_to_dot_scc`) when tracing is enabled; `dialyzer_dot` exists to support exactly this.
- The user-facing documentation (CLI options, warning descriptions, examples) lives in the `-moduledoc` of `dialyzer.erl` and in `../doc/`; the module docs of every other module are `-moduledoc false`.

## Dependencies
- From `dialyzer.app.src`: `applications = [compiler, kernel, stdlib]`; `runtime_dependencies = ["syntax_tools-2.0","stdlib-5.0","kernel-8.0","erts-12.0","compiler-10.0"]`.
- Build: `../vsn.mk` (`DIALYZER_VSN`), `$(ERL_TOP)/make/*.mk` (target/otp/dep/release rules), `erlc` and `sed`.
- Runtime use of OTP internals: `compile` with `core_v1` debug info, `beam_lib`, `cerl`/`prettypr` (syntax_tools), `digraph`, `sofs`, ETS.

## Test Strategy
- Tests are *not* here; they live in the sibling `../test/` directory (read-only from this node — escalate changes to the parent): `dialyzer_SUITE.erl` (main end-to-end suite), `dialyzer_cl_SUITE.erl`, `incremental_SUITE.erl`, `cplt_SUITE.erl`, `iplt_SUITE.erl`, `erl_types_SUITE.erl`, `abstract_SUITE.erl`, `dialyzer_utils_SUITE.erl`, `typer_SUITE.erl`, plus `dialyzer_common.erl`/`file_utils.erl` helpers and many `*_SUITE_data/` fixture trees.
- Suites are Common Test; fixture PLTs are rebuilt inside the suites, and `../test/dialyzer.spec` records the expected warning classes.
