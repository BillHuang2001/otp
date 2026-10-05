# differential — Rust ERTS vs System ERTS Compatibility Gate

## Intent
The in-repo **differential-test harness** (Stage 0; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §H).
For a given corpus case it runs the **real system ERTS** (the *oracle*) and the **Rust runtime** (`beam_rt`), then compares the **observable results**.
There are two kinds of case (`CaseKind`): **expression cases** evaluate the same self-terminating probe under both runtimes, and **module-run cases** compile an embedded Erlang module with `erlc` and run one exported function.

This is the *compatibility gate* of the rewrite (`../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I): no behavior change ships without a differential case.
It is a pure **driver/observer** — it never modifies the runtime and never inspects its internals.
The only contract with `beam_rt` is its documented external CLI (compat contract **A.6**).
It honors every shared convention in `../CONTEXT.md`.

Cargo **binary + library** crate named `differential`: the library exists so `cargo test` can assert the harness itself (oracle golden checks, oracle-vs-oracle equality, diff detection); the binary is what a human or CI drives.

## Observable results (what is compared)
Per case, each runtime is observed and compared field-by-field:
1. **stdout bytes** (exact, byte-wise) — always compared.
2. **exit status** (process exit code; `None` if signalled) — always compared.
3. **stderr bytes** — compared for **expression** cases (`Observables::All`), **not** for module-run cases (`Observables::StdoutAndExit`), because `beam_rt` prints a boot banner to stderr while `erl` prints nothing — that is not a semantic difference.
4. **`term_to_binary/1` of the evaluated result** — the probe *encodes the result* it observes, so the canonical ETF bytes are a first-class diff field (contract A.2).
5. **exception class + reason** — surfaced because `erl -noshell -eval` prints **nothing** on an uncaught error; the harness always wraps the expression in `try … catch` (see *Probe protocol*).
6. **selected `erlang:system_info/1` keys** — covered by corpus cases (contract A.8).

A Rust-side observation whose exit code is `3` (`runtime::UNSUPPORTED_EXIT_CODE`) is a documented **capability gap**, not a difference: it is classified `SideResult::Unsupported` carrying its first non-empty stderr line, so an unimplemented feature is `unsupported`, never a `mismatch`.

## Probe protocol (binding)
An **expression** case carries a plain Erlang expression, wrapped into a self-terminating probe:

- **`Probe::TermToBinary`** (default): `try (EXPR) of V -> io:format("ok ~w~n",[term_to_binary(V)]), halt(0) catch C:R -> io:format("exception ~w ~w~n",[C,R]), halt(1) end.`
- **`Probe::Value`**: same shape but prints `~w` of `V` (textual rendering, no ETF encode).

Success ⇒ one stdout line `ok <repr>`, exit `0`. Failure ⇒ one line `exception <class> <reason>`, exit `1`.
Expression cases may add **extra argv** (e.g. `-<flag> <value>`) to exercise CLI flag parsing (A.6), and their argv is built once and given to **both** runtimes: `["-noshell", <extra…>, "-eval", <probe>]`.

A **module-run** case (`CaseKind::Module(ModuleRun)`) drives the two runtimes with **different** argv:
- oracle: `["-noshell", "-pa", <dir>, "-eval", "io:format(\"~w~n\",[<mod>:<fun>(<args…>)]), halt(0)."]`.
- Rust runtime: `["run", "<dir>/<mod>.beam", "<mod>:<fun>/<arity>", <int-args…>]` (its `run` action).
The embedded source is compiled at run time with the system `erlc` into a unique temp dir (cleaned up afterwards); stdout + exit status only are compared.

## API Surface

### CLI (hand-rolled `std`-only arg parsing — **no `clap`**; `--help` usable on every subcommand)
- `differential run <case-id> [--json] [--out <dir>]` — run one corpus case both ways; print the comparison.
- `differential compare <expr> [--pa <dir>]… [--json] [--out <dir>]` — ad-hoc: evaluate an expression under both runtimes and compare.
- `differential suite [--strict] [--json] [--out <dir>]` — run the whole built-in corpus.
- `differential list [--by-contract] [--json] [--out <dir>]` — list corpus case ids + the contract each covers. `--by-contract` groups by contract id (A.1…A.9) with per-contract case counts and ids; without it the output is byte-for-byte the plain listing.
- `differential --help` / `-h`; `differential <sub> --help`.
- **Exit codes:** `0` all executed cases agreed (skips/`unsupported` reported, not fatal); `1` ≥1 **mismatch**; `2` usage/env error. `--strict` escalates `unsupported`/`skipped` to failure.

### Library (`src/lib.rs`) — the public API `cargo test` asserts against
- `error` — `Error` (`Usage`/`Env`/`Io`, all → exit `2`), `Result<T>`.
  Tool-level errors are host-tool errors; BEAM exceptions are *observed **data*** (never the harness's own failure mode), so `beam_common::BeamError` does not model this crate's failures.
- `cli` — `Command` enum (`Run{case_id,…}`, `Compare{expr,pa,…}`, `Suite{strict,…}`, `List{by_contract,json,out}`, `Help{…}`), `parse(args)`, `usage(topic)`.
- `runtime` — `UNSUPPORTED_EXIT_CODE` (`3`), `RuntimeId::{Oracle,Rust}`, `RuntimeSource::{Env,Path,WorkspaceDefault}`, `RuntimeSpec{id,program,source}`, `Observation{runtime,program,argv,stdout,stderr,exit_code}`, `resolve_oracle()`, `resolve_rust() -> Ok(Option<RuntimeSpec>)`, `eval_argv(expr,extra)`, `run(spec,argv)`.
- `module_build` — `MISSING_ERLC_REASON`, `resolve_erlc() -> Ok(Option<PathBuf>)`, `resolve_erlc_from(erlc,path)`, `CompiledModule{dir,beam}` (temp dir removed on drop), `compile(erlc,source,module)`.
- `compare` — `Observables::{All,StdoutAndExit}`, `SideResult::{Observed(Observation),Unsupported{reason}}`, `FieldDiff{field,oracle,rust}`, `GoldenCheck{matched,diffs}`, `Verdict::{Agree,Mismatch,Unsupported,Skipped}`, `compare_observations`, `compare_observations_with(o,r,observables)`, `verdict`, `verdict_with(o,r,golden,observables)`, `check_golden(&Observation,&Golden) -> GoldenCheck`.
- `corpus` — `Contract` (A.1–A.9 ids + labels), `Probe`, `Golden{stdout,exit_code}`, `CaseKind::{Eval{expr,extra_argv,probe},Module(ModuleRun)}`, `ModuleRun{source,module,function,arity,args}` (+ `oracle_eval`/`oracle_argv`/`rust_argv`/`beam_path`), `Case{id,description,contracts,kind,golden}` (+ `eval`/`kind_label`/`expr`/`extra_argv`), `cases()`, `find_case(id)`, `probe_expr(expr,probe)`.
- `harness` — `run_case(&Case) -> CaseOutcome`, `compare_expr(expr,pa) -> CaseOutcome`, `run_suite(strict) -> SuiteOutcome`; `CaseOutcome` carries per-side `oracle_argv`/`rust_argv` and derives the `Verdict` + exit code.
- `report` — `CaseReport`, `RunReport` (serde, stable `schema: "differential/1"`; carries `summary` **and** an additive top-level `by_contract` map of contract id → `Summary`), `Summary` (`total/agree/mismatch/unsupported/skipped`), `ListReport` (+ optional `by_contract` array of `ContractGroup{id,label,total,case_ids}`, present only via `from_corpus_grouped`), `ListEntry.probe` (the case kind label: probe name or `"module-run"`), `render_list_grouped`, `write_json`, `render_human`.

## Runtime resolution (shared convention; no baked absolute paths without an overridable default)
- **Oracle** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud `Error::Env` (exit 2).
- **Rust runtime** = `$BEAM_RT` if set (must exist, else `Error::Env`), else the workspace-relative default `<CARGO_MANIFEST_DIR>/../../target/{release,debug}/beam_rt`. Neither present ⇒ the Rust side reports `unsupported` (never a crash, never a fabricated pass).
- **Compiler (`erlc`)** = `$ERLC` if set (must exist, else `Error::Env`), else `erlc` resolved on `PATH`. Missing ⇒ `Ok(None)`: the module-run cases are **skipped loudly** (`Verdict::Skipped`), never a silent pass.
- Expression cases invoke both runtimes with the **same argv** (only the program path differs); module-run cases invoke them with per-side argv (see the probe protocol above).

## Constraints
- **Honest reporting:** a side that cannot run is `unsupported` / `skipped(<reason>)` — **never** `pass`, and oracle observations are **never** fabricated. A case whose *oracle* disagrees with its golden is itself a failure (corpus drift), surfaced as a mismatch — the harness is self-testing.
- **No coupling to `crates/*` internals** (crate deps are `serde` + `serde_json` only). No `clap`; arg parsing is hand-rolled `std`. `unsafe` is **forbidden**.
- **Loud failures:** stderr messages + nonzero exit; never silently swallow an error.
- Machine-readable JSON, stable schema, written under the gitignored `out/` (or `--out <dir>`).
- Files target ~1000 lines; split by responsibility. Apache-2.0.
- Keep the default corpus **small + fast** (smoke level); heavy suites are milestone-only (root §6).

## Design Decisions
- **Probe wrapper is mandatory** because `erl -noshell -eval '<expr with uncaught error>'` emits *no* stdout and exits 1 — without the wrapper, exceptions would be invisible to a stdout diff.
- **Classification is signal-based, never heuristic:** exit code `3` is `beam_rt`'s documented capability-gap signal, so `harness::classify_rust` maps it to `SideResult::Unsupported` (with the first non-empty stderr line). `mismatch` is reserved for a runtime that actually produced a differing observable result. `verdict_with` must use the *same* `Observables` as the reported `diffs`, so a case can never show an empty diff list yet be a `Mismatch`.
- **Two case kinds, two comparison modes.** Module-run cases compare stdout + exit status only: `beam_rt` writes a boot banner to stderr, which is not a semantic difference. Expression cases keep the full stdout + exit + stderr comparison.
- **Golden per case** lets the harness self-test: the oracle must reproduce it while the Rust side reports whatever it really does. If the oracle ever stops matching its golden, that is a real failure (drift), not a skip.
- **Volatile `system_info` keys use structural goldens** (`is_integer(process_count)` → `true`, `is_list(otp_release)` → `true`) and `version` → `<<"17.1">>` (the frozen erts target). `otp_release` is deliberately **not** asserted by exact value: the oracle is OTP 29 while the rewrite targets OTP 30, so an exact string would be a guaranteed false mismatch once the Rust runtime lands.
- **`--strict`** exists so CI can forbid `unsupported`/`skipped`; the default keeps today's capability gaps non-fatal.
- **A golden is a stdout *line*, without the trailing newline**: `compare::check_golden` normalizes the observation with `normalize_probe_stdout` (strips one trailing `\n`, and a preceding `\r`) before comparing it to `Golden::stdout`. This keeps a golden diff readable instead of hiding a newline inside it.
- **Child spawns retry a bounded `ETXTBSY` only.** Both production spawn sites (`runtime::run`, `module_build::compile`) go through `runtime::output_with_retry`. A `#!/bin/sh` script is legitimately executed here (`$ERTS_ORACLE`/`$BEAM_RT` may name a wrapper script; test stubs are scripts), and when one thread `fork()`s while another still holds a descriptor open **for writing** that script, the kernel transiently refuses the `exec` with `ETXTBSY` ("Text file busy", errno 26) — a pure scheduling artifact. The helper retries that one condition up to 5 attempts with a 10 ms backoff (matching `ErrorKind::ExecutableFileBusy`, and errno 26 under `#[cfg(unix)]`), then returns the **same** error so a surviving failure is still loud. Every other error is returned immediately, unchanged — this is a bounded retry of an understood transient race, not error swallowing.
- **Reports are always written** (`--out <dir>` sets the directory, default `out/`); `--json` only decides what goes to stdout. An ad-hoc `compare` asserts no golden, so its report carries `matched_golden: null` (the Rust side always does).

## Known Issues / Notes for Agents
- **Today's Rust side is a partial runtime:** every **expression** case is `unsupported` (the `-eval` grammar is not implemented — `beam_rt` exits `3`), while the **module-run** cases (A.2, `modules/add` + `modules/fib`) `agree`. `differential suite` therefore reports `agree=2 mismatch=0 unsupported=17` and exits `0`; `--strict` exits `1`.
- Build/test via the committed wrapper (read-only `$HOME`): `../../scripts/cargo.sh test --manifest-path ../../Cargo.toml -p differential`.
  Known deps (`serde`, `serde_json`) resolve **offline** — add no new crates.
- Verified oracle (system Erlang on `PATH`, OTP 29.1.1 / erts-17.1): `{ok,42}` under the default probe ⇒ `ok <<131,104,2,119,2,111,107,97,42>>`, exit 0.

## Test Strategy
- `#[cfg(test)]` in `cli` (arg parsing + `--help`), `compare` (pure diff + golden logic; `Observables::StdoutAndExit` ignores a stderr-only difference; differing fixtures ⇒ `Mismatch`), `corpus` (unique ids, every case has ≥1 contract, goldens well-formed per kind, the module cases' argv shape), `module_build` (erlc resolution, a failing compiler is a loud error, temp-dir cleanup), `harness` (exit-3 classification, the unsupported reason, the eval probe shape).
- `tests/oracle.rs` integration: **oracle-backed** — the oracle reproduces the `{ok,42}` golden; every **eval** case reproduces its golden; every **module-run** case reproduces its golden under the oracle (compiled with `erlc`); oracle-vs-oracle on the same case ⇒ `Verdict::Agree`; a Rust runtime forced to a nonexistent path ⇒ `unsupported` (never `pass`). These **skip with a loud note** (not a silent pass) when `erl`/`erlc` is unavailable, so `cargo test` stays honest everywhere.
- The transient-`ETXTBSY` retry is covered **deterministically** (no timing dependence): on Linux, `exec`ing a file that is held open for writing returns `ETXTBSY` at the VFS level, so the tests hold the descriptor open and assert the retry is bounded + surfaces the error (`runtime::tests::output_with_retry_is_bounded_on_etxtbsy_and_surfaces_it`, `module_build::tests::a_compiler_that_cannot_be_exec_yet_is_a_loud_error_not_a_hang`), that a non-`ETXTBSY` error is returned unchanged, and that a successful spawn passes through.
- `tests/cli.rs` drives the **binary** as a child process (so `$ERTS_ORACLE`/`$BEAM_RT` can be injected without touching the test process environment) and locks in the exit-code contract (`0`/`1`/`2`), the `--help` surface, the `--out` report file and the `list` path that needs no runtime. It also injects a fake `$BEAM_RT`: one printing a different stdout must still be `[MISMATCH]` (exit 1), one printing `beam_rt: unsupported: …` and exiting `3` must be `unsupported` (exit 0 lenient, `1` under `--strict`). Oracle-dependent cases skip loudly when `erl` is unavailable.
- Gate: `cargo test -p differential` GREEN and `differential suite --json` runs end-to-end (module cases `agree`, expression cases `unsupported`, no `mismatch`).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, public re-exports (the public API surface)
- `src/error.rs` → `Error` (`Usage`/`Env`/`Io`) + `Result` + exit-code mapping
- `src/cli.rs` → `Command` (incl. `list --by-contract`), hand-rolled `parse`, `--help`/usage text
- `src/runtime.rs` → runtime resolution (`$ERTS_ORACLE`/`erl`, `$BEAM_RT`/target), `UNSUPPORTED_EXIT_CODE`, `Observation`, argv build, `output_with_retry` (bounded transient-`ETXTBSY` spawn retry) + process spawn
- `src/module_build.rs` → `erlc` resolution (`$ERLC`/`$PATH`), embedded-module compilation into a temp dir (`CompiledModule`), spawning through `runtime::output_with_retry`
- `src/compare.rs` → observable diff (`Observables`), golden check, `Verdict` (`verdict`/`verdict_with`)
- `src/corpus/mod.rs` → `Case`/`CaseKind`/`ModuleRun`/`Contract`/`Probe`/`Golden` + registry (`cases`, `find_case`, `probe_expr`)
- `src/corpus/etf.rs` → `.beam`/ETF cases (A.2): `term_to_binary` bytes, `binary_to_term` round-trip
- `src/corpus/terms.rs` → language-semantics cases (A.1): term ordering/equality, arithmetic
- `src/corpus/bifs.rs` → BIF/OTP-app cases (A.7): `length`, `integer_to_binary`, `lists:reverse`
- `src/corpus/system_info.rs` → introspection cases (A.8): `system_info/1` keys
- `src/corpus/flags.rs` → CLI/config cases (A.6): `-noshell` + custom `init:get_argument` flags
- `src/corpus/modules.rs` → module-run cases (A.2): embedded `erlc`-compiled module agreement
- `src/harness.rs` → orchestration: `run_case`/`compare_expr`/`run_suite`, per-side argv planning, exit-3 classification, exit-code derivation
- `src/report.rs` → JSON report schema (serde) + writer + human rendering
- `src/main.rs` → thin binary: parse args → dispatch → `process::exit(code)`
- `tests/oracle.rs` → oracle-backed integration tests (golden reproduction for eval + module cases, oracle-vs-oracle, missing Rust runtime)
- `tests/cli.rs` → end-to-end binary tests (exit codes, `--help`, `--out` report, `list`, fake `$BEAM_RT` mismatch/unsupported)
- `out/` → gitignored JSON report output (default `--out`)

## See Also
- `../CONTEXT.md` — the shared conventions this crate must follow (runtime resolution, JSON, exit codes).
- `../../CONTEXT.md` §6 / §7 — the differential gate is the compatibility gate (Stage 0).
- `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §H — Stage 0 mandates this harness.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7 / §I — differential suite = the success-metric gate.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` — the contracts the corpus must exercise.
