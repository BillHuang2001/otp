# Dialyzer — DIscrepancy AnaLYZer (lib/dialyzer)

## Intent
The `dialyzer` OTP application: a static success-typing analyzer for Erlang/OTP that identifies definite discrepancies (type errors, unreachable/never-returning code, redundant tests, spec–success-typing mismatches) without reporting false positives.
Analyzes debug-compiled BEAM (`debug_info`) or Erlang source, both converted to Core Erlang and solved against a call graph.
Bases inference on the Persistent Lookup Table (PLT) — a cache of per-module types/contracts that is built from OTP libraries before analysis.
Also ships `typer` (TypEr), a separate CLI that annotates programs with inferred success typings using the same type machinery.
CLI + library only: there is NO GUI module (the wx GUI was removed); the app runs through the `dialyzer`/`typer` shell front ends or the `dialyzer` module API.

## API Surface
Public Erlang API — module `dialyzer` (declared in `src/dialyzer.erl`, the only cross-application entry point):
- `run/1` — run a command-line-like analysis; takes `dial_option()`s, returns `[dial_warning()]`.
- `run_report_modules_analyzed/1` / `run_report_modules_changed_and_analyzed/1` — same, additionally returning the analyzed (and changed) module lists.
- `plt_info/1` — return `{ok, ...}`/`{error, Reason}` info about a PLT file (classic or incremental).
- `format_warning/1,2` — string rendering of a `dial_warning()` (optionally with full paths).
- `plain_cl/0` — never-returning entry used only by the `dialyzer` C/shell front end (calls `erlang:halt/1`).
- `typer` (`src/typer.erl`) is a separate user tool (`start/0`), not part of the `dialyzer` module API.
Public types live in `src/dialyzer.hrl`: `dial_option()`, `dial_options()`, `dial_warning()`, `dial_warn_tag()`, `anal_type()`, `warn_option()`, and records `#options{}`/`#analysis{}`/`#plt{}`/`#plt_info{}`/`#iplt_info{}`.
Analysis types (`analysis_type` option) drive the whole PLT lifecycle: `succ_typings` (analyze), `plt_build`, `plt_add`, `plt_remove`, `plt_check`, and `incremental`.
Front ends: `bin/dialyzer`/`bin/typer` (shell/C) and `dialyzer:plain_cl/0`/`dialyzer_cl_parse` (CLI parsing), `dialyzer_cl` (CLI driver), `dialyzer_incremental` (incremental-mode driver).
The authoritative CLI/user documentation is the `-moduledoc` of `src/dialyzer.erl` (rendered as man3 `dialyzer.3`).

## PLT Model
Two on-disk PLT flavours share one in-memory implementation (`dialyzer_plt`) and are selected by `analysis_type`/options:
- **Classic PLT** (`dialyzer_cplt`): keyed by file path + MD5; not portable across machines; built with `--build_plt`/`--add_to_plt` and validated with `--check_plt`.
- **Incremental PLT** (`dialyzer_iplt`, `--incremental`): keyed by module name (portable) and caches per-module warnings plus the legal warning set.
`dialyzer_plt:plt_kind/1` distinguishes the on-disk kinds; the two modules intentionally duplicate each other and are expected to diverge.
Default PLT locations come from `DIALYZER_PLT`/`DIALYZER_IPLT` or the OS user cache dir; PLT compatibility is versioned by the dialyzer app version (`?VSN`) plus an implementation MD5 over key dialyzer modules.

## Constraints
- Depends on the compiler's Core Erlang and `debug_info`: BEAM input must be compiled with `+debug_info` (or analysis must start from source via `{from, src_code}`), otherwise dialyzer reports a recompile hint.
- `src/` compiles with `-Werror` and extra warnings (`+warn_export_vars +warn_unused_import +warn_missing_spec +warn_obsolete_bool_op`) — new warnings fail the build.
- `dialyzer.app` may declare only `applications: [compiler, kernel, stdlib]`; `syntax_tools` (used heavily via `cerl`) is a `runtime_dependencies`-only requirement.
- `ebin/` is a build-output dir (only `.gitignore` tracked); `plt/` is an empty legacy placeholder (only an empty `.gitignore` tracked) — the app's own self-dialyzer PLT (from `DIA_PLT_APPS = compiler syntax_tools` + defaults, see `Makefile`) is built by the `dialyzer_plt` target into `priv/plt/dialyzer.plt` (rule in `$(ERL_TOP)/make/app_targets.mk`, `DIA_PLT_DIR = ./priv/plt`), which does not exist in a plain checkout.
- Never edit generated `ebin/*.app`/`*.appup`/`.beam` by hand — edit `src/*.src` and regenerate (`%VSN%` substituted via `sed`).

## Design Decisions
- **Success typings**: types describe what a function *accepts/returns when it succeeds*, so warnings are sound and there are no false positives; specs are refined by success typings, not treated as ground truth.
- **Two solver engines** alternate to a fixpoint: `dialyzer_typesig` (constraint/graph-based per-SCC solving, with `v1`/`v2` solvers, `v2` default) and `dialyzer_dataflow` (finer module-by-module forward propagation that emits warnings), orchestrated by `dialyzer_succ_typings`.
- **Pipeline split**: callgraph construction (`dialyzer_analysis_callgraph`, `dialyzer_callgraph`, `dialyzer_dep`) → ETS-backed code server (`dialyzer_codeserver`, `dialyzer_clean_core`, `cerl_prettypr`) → SCC condensation + type solving → warnings.
- **Fractal parallelism**: `dialyzer_coordinator` schedules `compile`/`typesig`/`dataflow`/`warnings` (and contract/record remote-type) jobs across `dialyzer_worker` processes; Core/ETS state is moved (not copied) between processes.
- PLT I/O runs in a spawned helper (`subproc/1`) so a corrupt/foreign PLT cannot crash the analysis.

## Dependencies
- From `src/dialyzer.app.src`: `applications = [compiler, kernel, stdlib]`; `runtime_dependencies = ["syntax_tools-2.0","stdlib-5.0","kernel-8.0","erts-12.0","compiler-10.0"]`.
- Build inputs: `vsn.mk` (`DIALYZER_VSN = 6.0.3`), `$(ERL_TOP)/make/*.mk` (target/otp/dep/release/doc rules), `erlc`, `sed`.

## Test Strategy
- Common Test suites in `test/` (`*_SUITE.erl`), run with `make test` in this app dir (needs an installed release; installed as `$(RELEASE_PATH)/dialyzer_test` and driven by `test/dialyzer.spec`), or `ct_run` per suite/case.
- A shared test PLT is built/cached and lock-guarded in the CT run dir by `test/dialyzer_common.erl` (`check_plt/1`); PLT build/check can take up to `?plt_timeout` (2 hours), so suites must run sequentially.
- Hand-written suites (abstract, cplt, dialyzer, dialyzer_cl, dialyzer_utils, erl_types, iplt, incremental, typer) plus suites auto-generated from `*_SUITE_data/` dirs at release time — see `test/CONTEXT.md`.

## Notes for Agents
- No GUI: dialyzer is CLI/library only; do not look for a `dialyzer_gui_*` module.
- The user-facing CLI/API reference lives in `src/dialyzer.erl`'s moduledoc, not in `doc/` (which holds the guide chapter, typer reference, release notes, and legacy orphaned `about.txt`/`warnings.txt`).
- `src/dialyzer_dataflow.erl` (~3.8 kLOC) and `src/erl_types.erl` (~5.8 kLOC) are monoliths — prefer targeted reads over full-file reads.
- Adding/removing a module requires updating the explicit `MODULES` list in `src/Makefile` AND the `modules` list in `src/dialyzer.app.src`.

## Routing Table
- `src/` → the application's Erlang sources (public API `dialyzer`, CLI, callgraph, code server, solvers, PLT cplt/iplt/plt, `erl_types`/`erl_bif_types`, `typer`); see `src/CONTEXT.md`
- `test/` → Common Test suites + `*_SUITE_data` fixtures and the PLT-building helpers; see `test/CONTEXT.md`
- `doc/` → ExDoc/man sources (guide chapter, typer CLI reference, release notes `notes.md`); see `doc/CONTEXT.md`
- `plt/` → empty legacy placeholder dir (git-tracked only via an empty `.gitignore`); NOT where the app's PLT is built (that is `priv/plt/dialyzer.plt`, created at build time by `make dialyzer_plt`)
- `ebin/` → build output (`dialyzer.app`/`dialyzer.appup`/`.beam`; git-ignored, only `.gitignore` tracked)
- `Makefile`, `vsn.mk`, `AUTHORS`, `README`, `RELEASE_NOTES` → app build metadata and top-level docs at this node

## See Also
- `../CONTEXT.md` — parent `lib/` (OTP applications) and `../dialyzer/CONTEXT.md` sibling apps.
- `src/CONTEXT.md`, `test/CONTEXT.md`, `doc/CONTEXT.md` — child nodes.
