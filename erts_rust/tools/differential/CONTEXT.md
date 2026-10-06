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
3. **stderr bytes**, **classified** — compared for **expression** cases (`Observables::All`) through a narrow classifier: only the Rust runtime's documented one-line boot banner (`beam_rt <vsn> booted: …`, recognised by `compare::is_runtime_banner_line` / dropped by `compare::significant_stderr`) is ignored; every other stderr byte is still compared exactly. Module-run cases (`Observables::StdoutAndExit`) do not compare stderr at all.
4. **`term_to_binary/1` of the evaluated result** — the probe *encodes the result* it observes, so the canonical ETF bytes are a first-class diff field (contract A.2).
5. **exception class + reason** — surfaced because `erl -noshell -eval` prints **nothing** on an uncaught error; the harness always wraps the expression in `try … catch` (see *Probe protocol*).
6. **selected `erlang:system_info/1` keys** — covered by corpus cases (contract A.8).

A Rust-side observation is a documented **capability gap**, not a difference, in either of two ways — both classified `SideResult::Unsupported`, so an unimplemented feature is `unsupported`, never a `mismatch`:
1. its exit code is `3` (`runtime::UNSUPPORTED_EXIT_CODE`): the reason carries the runtime's first non-empty stderr line; or
2. a **probe-wrapped** (eval) case's probe *caught* the raise: the probe prints it as its single `exception <class> <reason>` stdout line and exits `1`, and `<reason>` (after stripping one pair of surrounding `'…'` quotes) follows the runtime's documented `unsupported` reason convention — the reason is recorded as `beam_rt: unsupported (caught by the probe): <stripped>`, stripped exactly as `beam_rt`'s `unsupported_reason` does. Detection is deliberately narrow (`harness::caught_unsupported_reason`): only the documented `unsupported` prefix (with a following `:`/`_`/whitespace, or nothing) triggers it, so a genuine Erlang reason (`badarg`, `system_limit`, `function_clause`, `'erlang:open_port/2 is not a BIF'`, …) stays an observation → `mismatch`. The gate is explicit: `RunPlan::probe_wrapped` is `true` only for `CaseKind::Eval`/`compare_expr` and `false` for module-run cases (whose stdout is `~w` of a result).

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
- **Exit codes (the binary):** `0` all executed cases agreed (skips/`unsupported` reported, not fatal); `1` ≥1 **mismatch**; `2` usage/env error. `--strict` escalates `unsupported`/`skipped` to failure. `gate.sh` (below) layers the `0/1/2/3` CI mapping on top; the binary's own contract is unchanged.
- `gate.sh` (executable, POSIX `sh`, repo-relative — no baked paths) — the **CI gate** wrapper. It builds `beam_rt` + `differential` via the committed `../../scripts/cargo.sh`, runs the corpus as `suite --strict --json --out <tmpdir>`, and maps the written `report.json` to its own exit code.

### Library (`src/lib.rs`) — the public API `cargo test` asserts against
- `error` — `Error` (`Usage`/`Env`/`Io`, all → exit `2`), `Result<T>`.
  Tool-level errors are host-tool errors; BEAM exceptions are *observed **data*** (never the harness's own failure mode), so `beam_common::BeamError` does not model this crate's failures.
- `cli` — `Command` enum (`Run{case_id,…}`, `Compare{expr,pa,…}`, `Suite{strict,…}`, `List{by_contract,json,out}`, `Help{…}`), `parse(args)`, `usage(topic)`.
- `runtime` — `UNSUPPORTED_EXIT_CODE` (`3`), `RuntimeId::{Oracle,Rust}`, `RuntimeSource::{Env,Path,WorkspaceDefault}`, `RuntimeSpec{id,program,source}`, `Observation{runtime,program,argv,stdout,stderr,exit_code}`, `resolve_oracle()`, `resolve_rust() -> Ok(Option<RuntimeSpec>)`, `resolve_rust_from(beam_rt,manifest_dir)`, `eval_argv(expr,extra)`, `run(spec,argv)`, `output_with_retry`.
- `module_build` — `MISSING_ERLC_REASON`, `resolve_erlc() -> Ok(Option<PathBuf>)`, `resolve_erlc_from(erlc,path)`, `CompiledModule{dir,beam}` (temp dir removed on drop), `compile(erlc,source,module)`.
- `compare` — `Observables::{All,StdoutAndExit}`, `SideResult::{Observed(Observation),Unsupported{reason}}`, `FieldDiff{field,oracle,rust}`, `GoldenCheck{matched,diffs}`, `Verdict::{Agree,Mismatch,Unsupported,Skipped}`, `compare_observations`, `compare_observations_with(o,r,observables)`, `verdict`, `verdict_with(o,r,golden,observables)`, `check_golden(&Observation,&Golden) -> GoldenCheck`.
- `corpus` — `Contract` (A.1–A.9 ids + labels), `Probe`, `Golden{stdout,exit_code}`, `CaseKind::{Eval{expr,extra_argv,probe},Module(ModuleRun)}`, `ModuleRun{source,module,function,arity,args}` (+ `oracle_eval`/`oracle_argv`/`rust_argv`/`beam_path`), `Case{id,description,contracts,kind,golden}` (+ `eval`/`kind_label`/`expr`/`extra_argv`), `cases()`, `find_case(id)`, `probe_expr(expr,probe)`.
- `harness` — `RuntimeContext{oracle,rust}`, `resolve_context()`, `run_case(&Case) -> CaseOutcome`, `run_case_in(&RuntimeContext,&Case) -> CaseOutcome`, `compare_expr(expr,pa) -> CaseOutcome`, `run_suite(strict) -> SuiteOutcome`; `CaseOutcome` carries per-side `oracle_argv`/`rust_argv` and derives the `Verdict` + exit code.
- `report` — `CaseReport`, `RunReport` (serde, stable `schema: "differential/1"`; carries `summary` **and** an additive top-level `by_contract` map of contract id → `Summary`), `Summary` (`total/agree/mismatch/unsupported/skipped`), `ListReport` (+ optional `by_contract` array of `ContractGroup{id,label,total,case_ids}`, present only via `from_corpus_grouped`), `ListEntry.probe` (the case kind label: probe name or `"module-run"`), `render_list_grouped`, `write_json`, `render_human`.

## Runtime resolution (shared convention; no baked absolute paths without an overridable default)
- **Oracle** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud `Error::Env` (exit 2).
- **Rust runtime** = `$BEAM_RT` if set (must exist, else `Error::Env`), else the workspace-relative default `<CARGO_MANIFEST_DIR>/../../target/{release,debug}/beam_rt`. Neither present ⇒ the Rust side reports `unsupported` (never a crash, never a fabricated pass).
  - The default profile order is **`release` before `debug` on purpose** (the real CLI and `gate.sh` want the fastest artifact present). That order is wrong under `cargo test`, where only `debug` is rebuilt — so the **integration tests inject `$BEAM_RT`** pointing at the freshly built debug runtime instead of relying on it (see *Design Decisions*). The order itself is never changed.
- **Compiler (`erlc`)** = `$ERLC` if set (must exist, else `Error::Env`), else `erlc` resolved on `PATH`. Missing ⇒ `Ok(None)`: the module-run cases are **skipped loudly** (`Verdict::Skipped`), never a silent pass.
- Expression cases invoke both runtimes with the **same argv** (only the program path differs); module-run cases invoke them with per-side argv (see the probe protocol above).

## gate.sh — the checkable compatibility gate
`gate.sh` wraps `differential suite --strict` for CI. From the repo root it runs
`erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt -p differential`, then
`erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q -p differential -- suite --strict --json --out <tmpdir>`,
and turns the report into a CI exit code:

| Exit | Meaning |
|------|---------|
| `0`  | every executed case agreed |
| `1`  | ≥1 **mismatch** — a real, observable behavioral divergence (including oracle-vs-its-own-golden corpus drift) |
| `2`  | usage / environment error — missing oracle, build failure, or a report that could not be produced |
| `3`  | no mismatch, but ≥1 **unsupported**/`skipped` capability gap remains |

**Invariant:** an `unsupported` case (a capability gap the Rust runtime signals via its documented exit `3` / `beam_rt: unsupported: …` reason convention, or a missing `beam_rt`) is **never** reported as a mismatch. Exit `1` is reserved for a case whose runtime actually produced a *differing observable result*; a capability gap can only ever land in the `unsupported`/`skipped` buckets and therefore cause exit `3` (never exit `1`).

The `1`-vs-`3` distinction is read **solely** from the report's **top-level `summary`** object (authoritative); the per-contract `by_contract` rollup — which also carries `mismatch` keys, at a deeper indent — is deliberately ignored. If the summary cannot be parsed, the gate **fails loud** (exit `1`) rather than risk a false pass (`0`).

The binary's own `run`/`suite` exit codes are unchanged (`0` agree / `1` mismatch / `2` usage-env; a lenient `run` on an unsupported case stays `0`) — the `0/1/2/3` mapping belongs to the gate script only.

## Constraints
- **Honest reporting:** a side that cannot run is `unsupported` / `skipped(<reason>)` — **never** `pass`, and oracle observations are **never** fabricated. A case whose *oracle* disagrees with its golden is itself a failure (corpus drift), surfaced as a mismatch — the harness is self-testing.
- **No coupling to `crates/*` internals** (crate deps are `serde` + `serde_json` only). No `clap`; arg parsing is hand-rolled `std`. `unsafe` is **forbidden**.
- **Loud failures:** stderr messages + nonzero exit; never silently swallow an error.
- Machine-readable JSON, stable schema, written under the gitignored `out/` (or `--out <dir>`).
- Files target ~1000 lines; split by responsibility. Apache-2.0.
- Keep the default corpus **small + fast** (smoke level); heavy suites are milestone-only (root §6).
- **Never assert a golden whose output is order-unstable.** A `~w` rendering of a multi-key map (or any structure containing one) has a per-VM-start iteration order, so compare it structurally (`maps:size`, `lists:sort(maps:keys(M))`, single-key maps) instead. Same rule for pids/refs/ports/creation values that vary per VM start — assert a shape, or use a deterministic decode→re-encode round-trip.

## Design Decisions
- **Probe wrapper is mandatory** because `erl -noshell -eval '<expr with uncaught error>'` emits *no* stdout and exits 1 — without the wrapper, exceptions would be invisible to a stdout diff.
- **Classification is signal-based, never heuristic:** the two `beam_rt` capability-gap signals — exit code `3` (a raise that escaped the program) and a probe-caught raise printed as exactly one `exception <class> <reason>` line with exit `1` whose reason follows the `unsupported` convention — are mapped by `harness::classify_rust` to `SideResult::Unsupported` (with the runtime's stderr line, or the stripped caught reason). `mismatch` is reserved for a runtime that actually produced a differing observable result. `verdict_with` must use the *same* `Observables` as the reported `diffs`, so a case can never show an empty diff list yet be a `Mismatch`.
- **Two case kinds, two comparison modes.** Module-run cases compare stdout + exit status only. Expression cases add a **classified** stderr comparison: `significant_stderr` drops only the runtime's documented boot-banner line (`beam_rt <vsn> booted: …`) from both sides before the byte diff, so `beam_rt`'s banner cannot turn a legitimate `-eval` agreement into a mismatch while any *other* stderr difference stays a hard mismatch. The classifier is narrow by construction (a `beam_rt ` prefix **and** a ` booted: ` marker), so the `beam_rt: unsupported: …` capability-gap line and arbitrary `beam_rt …` diagnostics stay significant — it can only ever drop a known banner, never fabricate agreement. `compare_observations_with` reports the classified buffers, so a `stderr` `FieldDiff` shows the significant difference, not the banner noise.
- **Golden per case** lets the harness self-test: the oracle must reproduce it while the Rust side reports whatever it really does. If the oracle ever stops matching its golden, that is a real failure (drift), not a skip.
- **Volatile `system_info` keys use structural goldens** (`is_integer(process_count)` → `true`, `is_list(otp_release)` → `true`) and `version` → `<<"17.1">>` (the frozen erts target). `otp_release` is deliberately **not** asserted by exact value: the oracle is OTP 29 while the rewrite targets OTP 30, so an exact string would be a guaranteed false mismatch once the Rust runtime lands.
- **`--strict`** exists so CI can forbid `unsupported`/`skipped`; the default keeps today's capability gaps non-fatal.
- **A golden is a stdout *line*, without the trailing newline**: `compare::check_golden` normalizes the observation with `normalize_probe_stdout` (strips one trailing `\n`, and a preceding `\r`) before comparing it to `Golden::stdout`. This keeps a golden diff readable instead of hiding a newline inside it.
- **Child spawns retry a bounded `ETXTBSY` only.** Both production spawn sites (`runtime::run`, `module_build::compile`) go through `runtime::output_with_retry`. A `#!/bin/sh` script is legitimately executed here (`$ERTS_ORACLE`/`$BEAM_RT` may name a wrapper script; test stubs are scripts), and when one thread `fork()`s while another still holds a descriptor open **for writing** that script, the kernel transiently refuses the `exec` with `ETXTBSY` ("Text file busy", errno 26) — a pure scheduling artifact. The helper retries that one condition up to 6 attempts with a **doubling, capped** backoff (5 → 10 → 20 → 40 → 80 ms = a 155 ms total window; matching `ErrorKind::ExecutableFileBusy`, and errno 26 under `#[cfg(unix)]`), then returns the **same** error so a surviving failure is still loud. Every other error is returned immediately, unchanged, and the window is bounded so a permanent failure is surfaced promptly — this is a bounded retry of an understood transient race, not error swallowing.
- **Integration tests inject the freshly built debug runtime.** `tests/common/mod.rs::fresh_rust_runtime()` returns the `beam_rt` sitting next to `CARGO_BIN_EXE_differential` (Cargo builds both into the same profile dir), and every test that exercises the real runtime injects it as `$BEAM_RT` (or builds a `harness::RuntimeContext{ oracle, rust: fresh_rust_spec() }`), so a **stale `target/release/beam_rt` can never shadow the fresh debug build** and produce a bogus mismatch. `CARGO_BIN_EXE_beam_rt` would be the direct way to name it, but Cargo only defines that for binaries of the package under test, so it is not available here — the sibling lookup is exact, not heuristic.
- **Test scratch dirs are worktree-local, never the ambient `$TMPDIR`.** `tests/common/mod.rs::scratch_root()` returns `CARGO_TARGET_TMPDIR` (`<worktree>/erts_rust/target/tmp`, private to this build), and `tests/cli.rs` roots both its `Scratch` dirs and the child's own `TMPDIR` there, so the tests do not depend on a shared/volatile system temp dir (a sandbox wiped `$TMPDIR` mid-run and flaked a fake-`$BEAM_RT` spawn with ENOENT). `tests/oracle.rs` deliberately keeps `std::env::temp_dir()` where it is used as a `manifest_dir` *outside* the worktree's `target/` (a `CARGO_TARGET_TMPDIR` base there would make the "absent Rust runtime" case find a real runtime).
- **Reports are always written** (`--out <dir>` sets the directory, default `out/`); `--json` only decides what goes to stdout. An ad-hoc `compare` asserts no golden, so its report carries `matched_golden: null` (the Rust side always does).
- **The per-contract rollup is purely additive and derived.** `RunReport.by_contract` maps each contract id present in the run to the same `Summary` shape as the top-level `summary` (a case that declares several contracts contributes to each, so the per-contract `total`s need not sum to `summary.total`). The `list` listing only carries `by_contract` when `--by-contract` is given (`serde(skip_serializing_if = "Option::is_none")`), so the plain `list` output — human text and `differential/list/1` JSON — is byte-for-byte unchanged. Schema ids are **not** bumped: both changes only add keys.
- **`erlc` compile results are memoized** in a process-local cache (`module_build`, keyed by erlc path + module + exact source). The corpus's module-run cases share only a couple of embedded sources, so this replaces dozens of `erlc` spawns with a handful while every case still gets its **own** temp dir and unchanged argv. The cache stores the compiled `.beam` **bytes**, not a `CompiledModule`, precisely so each `CompiledModule` keeps removing its own temp dir on drop (the contract `module_build`'s tests pin). A failed/absent compiler is never cached, so a compile failure stays a loud error on every call.
- **"Modules execute", not "modules load".** The crate-level *load* sweeps (`../crates/beam_loader/tests/real_modules.rs`, `../crates/beam_engine/tests/stdlib_sweep.rs`) prove real `.beam`s decode; this harness adds the **execute** dimension two ways: the `mexec` corpus group (a curated set of tiny `erlc`-compiled wrappers whose int-arg functions call a real system module — `calendar`, `ordsets`, `queue`, `gb_trees`, … — so `beam_rt run` must load *and* execute real OTP code) and the broader `tests/module_sweep.rs` (kept out of the default `suite` to stay fast). Every outcome must be `Agree` or `Unsupported` — never `Mismatch`.
- **The A.3/A.4 structural C-ABI check is load-bearing.** `tests/abi.rs` resolves the target's pointer size from `__SIZEOF_POINTER__` (a compiler-predefined macro) and synthesizes `erl_int_sizes_config.h`; it then `-fsyntax-only`-compiles a C TU that references the `enif_*`/`driver_*`/`erl_drv_*` surface **and** `_Static_assert`s the frozen `ERL_NIF_TERM`/struct sizes+offsets mirrored from `../crates/beam_abi` (read-only), plus self-tests that a bogus symbol and a wrong frozen size both fail. It skips **loudly** only when no C compiler / headers / integer sizes are discoverable.
- **`gate.sh` layers the CI exit codes on the binary.** The binary keeps its `0/1/2` contract (and lenient `run` stays `0` for an unsupported case); the script reads the report's top-level `summary` and maps mismatch→`1`, unsupported/skipped→`3`. See the *gate.sh* section.

## Known Issues / Notes for Agents
- **The Rust side is a partial runtime; today's gate is 0 mismatches** (431-case corpus). The **module-run** cases (embedded sources, the native-record module group, and the real-system-module `mexec` group) `agree`, and the **eval** cases `agree` where the `-eval` evaluator supports them — `beam_rt` has a real Erlang-expression evaluator and does **not** auto-print the result. A capability gap is `unsupported`, never a mismatch: either the raise escapes and the CLI exits `3` (`beam_rt: unsupported: …`), or (for a probe-wrapped case) the probe *catches* it and prints one `exception <class> <reason>` line whose reason follows the `unsupported` convention.
  - Current rollup (**stable across repeated runs** — verified byte-identical over 6 consecutive full-suite runs): **431 total · 423 agree · 0 mismatch · 8 unsupported · 0 skipped** (`gate.sh` → exit `3`).
  - Per-contract rollup (A.1–A.9; a case may declare several contracts, so the column totals exceed `summary.total`):

| Contract | total | agree | mismatch | unsupported | skipped |
|---|---|---|---|---|---|
| A.1 language semantics | 118 | 118 | 0 | 0 | 0 |
| A.2 .beam binary/ETF bytes | 134 | 134 | 0 | 0 | 0 |
| A.3 NIF C ABI | 18 | 18 | 0 | 0 | 0 |
| A.4 driver C ABI / port | 37 | 37 | 0 | 0 | 0 |
| A.5 distribution wire protocol | 62 | 58 | 0 | 4 | 0 |
| A.6 CLI flags and configuration | 16 | 16 | 0 | 0 | 0 |
| A.7 OTP application contracts | 128 | 127 | 0 | 1 | 0 |
| A.8 introspection / system_info | 9 | 9 | 0 | 0 | 0 |
| A.9 build and bootstrapping | 52 | 49 | 0 | 3 | 0 |

- **The 10 `unsupported` cases (all honest capability gaps, none a mismatch).** Their goldens are the oracle's own output, so each flips to `agree` automatically once the gap closes:
  - **A.4 (1) — a port write BIF:** `driver/port_command_write_read` (`beam_rt: unsupported: erlang:port_command/2 is a known BIF that is not implemented`).
  - **A.5 (4) — encode-side codec gaps:** `wire/export_ext_encode` and `wire/new_fun_ext_encode` (`beam_rt: unsupported: local_fun`), `wire/compressed_option_encode` and `wire/compressed_option_roundtrip` (`term_to_binary(T,[compressed])`; `beam_rt: unsupported: bif`). Decode-side coverage of the same tags `agree`.
  - **A.9 (5) — process spawn / messaging:** `boot/receive_message_from_spawned_process`, `boot/receive_computed_value_from_spawned_process`, `boot/receive_pattern_match_from_spawned_process`, `boot/receive_ping_pong_between_processes` (`beam_rt: unsupported: erlang:spawn/1 is a known BIF that is not implemented`) and `boot/spawn_link_notifies_caller` (`… erlang:spawn_link/1 …`).
- **`--strict`** exits `1` while any `unsupported`/`skipped`/`mismatch` remains; `tests/cli.rs` pins that a lenient (`run`) invocation on an unsupported case stays exit `0`.
- **Real runtime divergences verified while authoring the corpus — deliberately NOT in the corpus (each yields a *plain Erlang value/reason*, so it would be a `mismatch`; handoffs to the runtime nodes).** They close automatically if the runtime signals the gap via its documented exit `3` / `unsupported` reason convention instead:
  - **Port surface (`beam_rt`):** `port_info/2` returns the bare value where erts returns `{Item,Value}` (`{name,"true"}` vs `"true"`, `{os_pid,N}` vs `N`, `{connected,<pid>}` vs `true`, `{input,0}`/`{output,0}` vs `undefined`); `port_info(P)` after `port_close/1` is `undefined` in the oracle but `badarg` in the Rust runtime; `proplists:get_value(links, port_info(P))` is `[<pid>]` vs `[]`, `id` is an integer vs `badarg`, and `lists:keymember(monitors,1,…)` is `false` vs `true`; the `exit_status` option yields `{Port,{exit_status,0}}` vs `no_status`; `erlang:ports/0` is `[#Port<0.0>]` vs `[]`; `erlang:system_info(port_count)` is `1` vs `0`.
  - **Wire codec (`beam_codec`):** legacy `PORT_EXT` (102) preserves the port id in the oracle but zeroes it in the Rust runtime; `V4_PORT_EXT` (121) decodes in neither but with different reasons (`badarg` vs `system_limit`); `{minor_version,1}` atom encoding emits legacy `ATOM_EXT` (100) in the oracle but `SMALL_ATOM_UTF8_EXT` (119) in the Rust runtime; a multi-key-map encode emits the keys in a different (semantically equal) order; a `compressed` (80) blob decompresses in the oracle but raises `badarg` in the Rust runtime. `FUN_EXT` (115) is not covered at all — the reference decoder does not validate it, so any golden would pin an unspecified artifact.
  - **Code server / process surface (`beam_rt`):** `code:all_loaded/0`'s second element is an **atom** in the Rust runtime but a **path string** in the oracle; `erlang:group_leader/0` is `<0.70.0>` vs `<0.0.0>`; `erlang:module_loaded(lists)` is `true` vs `false`; `string:to_integer("42")` is `{42,[]}` vs `exception error undefined`; `string:pad_integer/3` raises `undef` vs `'… is not exported'`.
- **What the driver requires of `beam_rt` for an expression case to reach `agree`.** The eval cases spawn `beam_rt -noshell <extra…> -eval <probe>` and compare **stdout + exit + stderr** against the oracle (module-run cases compare stdout + exit only). `erl -eval` does **not** echo the evaluated value — the probe prints the single `ok …` / `exception <class> <reason>` line itself via `io:format/2` and terminates with `halt/1`, so `beam_rt -eval` must likewise **not** auto-print the result (a second line breaks `Agree`), and must evaluate the whole `try (EXPR) of V -> … catch C:R -> … end.` wrapper (single-line, self-terminating). The module-run side instead uses `beam_rt run <file.beam> <mod>:<fun>/<arity> <args…>`, which must print `~w` of the result plus a trailing newline (the oracle prints the same via its `io:format("~w~n",…)`).
- **Residual: `src/module_build.rs` compiled-module temp dirs still use `std::env::temp_dir()`** (`unique_temp_dir`). That is normal production behaviour, but it means the **in-process** tests (`tests/oracle.rs`, `tests/module_sweep.rs`) place `.beam` temp dirs under the ambient `$TMPDIR`. Run them with a private `TMPDIR` in an environment whose `$TMPDIR` is shared/cleaned (the `tests/cli.rs` child processes are already independent of it — see *Design Decisions*).
- Build/test via the committed wrapper (read-only `$HOME`): `../../scripts/cargo.sh test --manifest-path ../../Cargo.toml -p differential`.
  Known deps (`serde`, `serde_json`) resolve **offline** — add no new crates.
- Verified oracle (system Erlang on `PATH`, OTP 29.1.1 / erts-17.1): `{ok,42}` under the default probe ⇒ `ok <<131,104,2,119,2,111,107,97,42>>`, exit 0.

## Test Strategy
- `#[cfg(test)]` in `cli` (arg parsing + `--help`), `compare` (pure diff + golden logic; stderr classification — a banner-only difference agrees while any other stderr line is a mismatch; the `beam_rt: unsupported: …` line is not a banner; `Observables::StdoutAndExit` ignores a stderr-only difference; the probe's `term_to_binary` bytes and exception class+reason are first-class stdout observables; the golden check ignores stderr; differing fixtures ⇒ `Mismatch`), `corpus` (unique ids, every case has ≥1 contract, goldens well-formed per kind, the module cases' argv shape), `module_build` (erlc resolution, a failing compiler is a loud error, temp-dir cleanup), `harness` (exit-3 classification, the unsupported reason, the eval probe shape, **and the caught-capability-gap classifier**: a quoted `'unsupported_opcode: …'` and a bare `unsupported_bif` are `Unsupported` with the stripped reason, while `badarg`, `system_limit`, a free-form `'erlang:open_port/2 is not a BIF'`, a two-line/non-probe stdout, a bad class, a missing reason, a `0` exit, a missing trailing newline and a non-probe-wrapped (`probe_wrapped=false`) case all stay observations; an `Unsupported` side never yields `Agree`).
- `tests/common/mod.rs` is the shared support module for the integration tests (a `tests/` subdirectory module, so it is never compiled as its own test binary). It exposes `fresh_rust_runtime()` / `fresh_rust_spec()` (the fresh debug `beam_rt`, so no test depends on a stale release build) and `scratch_root()` (a worktree-local `CARGO_TARGET_TMPDIR` scratch root).
- `tests/oracle.rs` integration: **oracle-backed** — the oracle reproduces the `{ok,42}` golden; every **eval** case reproduces its golden; every **module-run** case reproduces its golden under the oracle (compiled with `erlc`); oracle-vs-oracle on the same case ⇒ `Verdict::Agree`; a Rust runtime forced to a nonexistent path ⇒ `unsupported` (never `pass`). It builds an explicit `RuntimeContext` with the fresh runtime so it cannot observe a stale release build. These **skip with a loud note** (not a silent pass) when `erl`/`erlc` is unavailable, so `cargo test` stays honest everywhere.
- The transient-`ETXTBSY` retry is covered **deterministically** (no timing dependence): on Linux, `exec`ing a file that is held open for writing returns `ETXTBSY` at the VFS level, so the tests hold the descriptor open and assert the retry is bounded + surfaces the error (`runtime::tests::output_with_retry_is_bounded_on_etxtbsy_and_surfaces_it`, `module_build::tests::a_compiler_that_cannot_be_exec_yet_is_a_loud_error_not_a_hang`), **and** that it *recovers* once the writer releases (`runtime::tests::output_with_retry_recovers_once_the_transient_writer_closes`), that a non-`ETXTBSY` error is returned unchanged, and that a successful spawn passes through. `tests/cli.rs::fake_runtime` writes its fake `$BEAM_RT` through an explicit `File` that is flushed, `sync_all`'d and **closed** before it can be executed, removing the write-open race at its source.
- `tests/cli.rs` drives the **binary** as a child process (so `$ERTS_ORACLE`/`$BEAM_RT`/`TMPDIR` can be injected without touching the test process environment) and locks in the exit-code contract (`0`/`1`/`2`), the `--help` surface, the `--out` report file and the `list` path that needs no runtime. Every default-resolution invocation injects `$BEAM_RT` = the fresh debug runtime. It also injects a fake `$BEAM_RT`: one printing a different stdout must still be `[MISMATCH]` (exit 1), one printing `beam_rt: unsupported: …` and exiting `3` must be `unsupported` (exit 0 lenient, `1` under `--strict`). Oracle-dependent cases skip loudly when `erl` is unavailable.
- The suite assertion **never pins the current capability-gap count**: it checks the state-independent invariant that every case falls into exactly one verdict bucket (`agree + unsupported + skipped + mismatches == summary.total`) plus the branch-specific `agree >= 1` / `agree == 0` (when a usable runtime + `erlc` exist / do not), so the test stays true both today and once the gate is fully green.
- Gate: `cargo test -p differential` GREEN and `differential suite --json` runs end-to-end with every mismatching id listed in the `KNOWN_RUNTIME_GAPS` allowlist (an *unexpected* mismatch fails the test; today there are **zero** mismatches, so the allowlist holds no live entry). The CI gate is `gate.sh` (see its section).
- `tests/abi.rs` covers A.3/A.4 structurally: it synthesizes `erl_int_sizes_config.h` from `cc -dM -E -x c /dev/null` (pointer size from the compiler-predefined `__SIZEOF_POINTER__`) and `-fsyntax-only`-compiles a C TU referencing the frozen `enif_*`/`driver_*`/`erl_drv_*` surface **and** `_Static_assert`ing the frozen type sizes / key field offsets mirrored from `../crates/beam_abi`; self-tests prove a bogus symbol **and** a wrong frozen size both fail the check. It **skips loudly** when no `cc`/`gcc`/`clang` is available (no `crates/*` dependency, no `unsafe`).
- `tests/module_sweep.rs` is the broad real-module **execute** sweep (72 entries across 14 real system modules), driven through `differential::harness` under both runtimes with an explicit fresh-runtime `RuntimeContext`; it asserts every outcome is `Agree`/`Unsupported` (never `Mismatch`), prints a per-module rollup, and skips loudly without the oracle/`erlc`.
- The `corpus` registry tests assert all nine contracts A.1–A.9 are represented and that ≥20 module-run (genuine-agreement) cases exist.
- The per-contract rollup and the `list --by-contract` view are covered by `#[cfg(test)]` unit tests in `report.rs` (rollup counts per contract, a multi-contract case, the additive top-level JSON key, the contract-free `compare` rollup, and the grouped listing keeping the plain output unchanged) and in `cli.rs` (arg parsing for `--by-contract` and `--strict`, including that neither flag leaks onto a subcommand that does not take it).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, public re-exports (the public API surface)
- `src/error.rs` → `Error` (`Usage`/`Env`/`Io`) + `Result` + exit-code mapping
- `src/cli.rs` → `Command` (incl. `list --by-contract`), hand-rolled `parse`, `--help`/usage text
- `src/runtime.rs` → runtime resolution (`$ERTS_ORACLE`/`erl`, `$BEAM_RT`/target, release-before-debug), `UNSUPPORTED_EXIT_CODE`, `Observation`, argv build, `output_with_retry` (bounded transient-`ETXTBSY` spawn retry) + process spawn
- `src/module_build.rs` → `erlc` resolution (`$ERLC`/`$PATH`), embedded-module compilation into a temp dir (`CompiledModule`), a process-local compile cache (keyed by erlc path + module + source; caches the `.beam` bytes, not the temp dir), spawning through `runtime::output_with_retry`
- `src/compare.rs` → observable diff (`Observables`), classified stderr (`is_runtime_banner_line`/`significant_stderr`), golden check, `Verdict` (`verdict`/`verdict_with`)
- `src/corpus/mod.rs` → `Case`/`CaseKind`/`ModuleRun`/`Contract`/`Probe`/`Golden` + registry (`cases`, `find_case`, `probe_expr`)
- `src/corpus/etf.rs` → `.beam`/ETF cases (A.2): `term_to_binary` bytes, `binary_to_term` round-trip
- `src/corpus/terms.rs` → language-semantics cases (A.1): term ordering/equality, arithmetic
- `src/corpus/semantics.rs` → language-semantics cases (A.1): arithmetic/precedence, bignum, `==`/`=:=`/`=/=`, term ordering, `case`/`if`/`try`, funs, comprehensions, maps/tuples/binaries
- `src/corpus/errors.rs` → exception-protocol cases (A.1): `throw`/`exit`/`error` classes + reasons, common `badarg`/`badarith`/`function_clause` failures
- `src/corpus/bifs.rs` → BIF/OTP-app cases (A.7): `length`, `integer_to_binary`, `lists:*`, `maps:get`, binary ops, `io_lib:format`
- `src/corpus/system_info.rs` → introspection cases (A.8): `system_info/1` key shapes + badarg-on-unknown-key
- `src/corpus/flags.rs` → CLI/config cases (A.6): `init:get_argument/1`, `init:get_plain_arguments/0`, `-pa`
- `src/corpus/nif.rs` → observable NIF error surface (A.3): `erlang:nif_error/1` reason shapes (atoms, tuples, nested, integers, lists)
- `src/corpus/driver.rs` → observable driver/port surface (A.4): `os:cmd/1` round-trips, `open_port/2` + its binary/list `stream` modes, the spawn-driver read path, `port_info/1` (pair shape + single-key lookups), `ports/0`; `port_command/2` is an honest `unsupported` until the write path lands
- `src/corpus/wire.rs` → distribution/ETF wire cases (A.5): the ETF tags the wire codec uses — floats, int/small/large bignums, NIL/STRING/LIST (incl. improper tail)/BINARY, BIT_BINARY, small/large tuples, UTF-8 + legacy atoms, MAP_EXT, pid/ref/port (+ their legacy→modern upgrades), EXPORT_EXT/NEW_FUN_EXT, `compressed`, `{minor_version,_}`, version framing, atom-cache-without-context (a live handshake / `DOP_*` framing is a documented gap)
- `src/corpus/boot.rs` → build/boot cases (A.9): boot/version + code-server + `application:*` observability (`system_info` shapes, `code:get_path/0`, `code:which/1`, `code:lib_dir/1`, `code:is_loaded/1`, `code:ensure_loaded/1`, `code:is_sticky/1`, `code:all_loaded/0`, `init:get_status/0`, `code:root_dir/0`, `application:get_env/*`, `loaded_applications/0`, …) plus the `spawn`/`receive` messaging cases (currently `unsupported`)
- `src/corpus/modules.rs` → module-run cases (A.2/A.1/A.7): embedded `erlc`-compiled modules run under both runtimes (the genuine-agreement core)
- `src/corpus/mexec.rs` → "modules execute" sweep group (A.2/A.7): tiny `erlc`-compiled wrappers whose int-arg functions call real system stdlib/kernel modules (`calendar`, `ordsets`, `queue`, `gb_trees`, `gb_sets`, `proplists`, `orddict`, `array`, `string`, `unicode`, `io_lib`, `filename`, `erl_scan`, `base64`) end-to-end under both runtimes
- `src/harness.rs` → orchestration: `RuntimeContext`/`resolve_context`, `run_case`/`run_case_in`/`compare_expr`/`run_suite`, per-side argv planning, exit-3 classification, exit-code derivation
- `src/report.rs` → JSON report schema (serde: `summary` + additive `by_contract` rollup; the `list` listing + optional `--by-contract` grouping) + writer + human rendering
- `src/main.rs` → thin binary: parse args → dispatch → `process::exit(code)`
- `tests/common/mod.rs` → shared integration-test support: `fresh_rust_runtime()`/`fresh_rust_spec()` (never observe a stale release build) + `scratch_root()` (worktree-local scratch)
- `tests/abi.rs` → A.3/A.4 structural C-ABI check: `-fsyntax-only`-compiles a tiny C TU referencing the `enif_*`/`driver_*`/`erl_drv_*` surface and `_Static_assert`ing the frozen type sizes/offsets, with a synthesized `erl_int_sizes_config.h` (skips loudly without a C compiler; no `crates/*` dependency)
- `tests/module_sweep.rs` → the broad real-module **execute** sweep (72 entries / 14 real system modules) — asserts `Agree`/`Unsupported`, never `Mismatch`; skips loudly without the oracle
- `tests/oracle.rs` → oracle-backed integration tests (golden reproduction for eval + module cases, oracle-vs-oracle, missing Rust runtime)
- `tests/cli.rs` → end-to-end binary tests (exit codes, `--help`, `--out` report, `list`, injected fresh `$BEAM_RT`, fake `$BEAM_RT` mismatch/unsupported, boot-banner-vs-real-stderr classification, the verdict-bucket invariant)
- `gate.sh` → the CI gate wrapper over `suite --strict` implementing the `0/1/2/3` exit-code contract
- `out/` → gitignored JSON report output (default `--out`)

## See Also
- `../CONTEXT.md` — the shared conventions this crate must follow (runtime resolution, JSON, exit codes).
- `../../CONTEXT.md` §6 / §7 — the differential gate is the compatibility gate (Stage 0).
- `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §H — Stage 0 mandates this harness.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7 / §I — differential suite = the success-metric gate.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` — the contracts the corpus must exercise.
