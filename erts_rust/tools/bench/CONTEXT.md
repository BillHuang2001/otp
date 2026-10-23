# Benchmark Driver (`erts_rust/tools/bench`) — `bench` crate

## Intent
`bench` is the in-repo **benchmark driver** for the Rust ERTS rewrite.
It builds and runs the external pure-Erlang/Elixir target workloads named in `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` and **always records the system-ERTS baseline**, capturing the success metrics mandated by `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I: **startup latency, p50/p99 tail latency, throughput, peak memory**.
It is a **library + binary** crate: the binary (`bench`) is what a human or CI drives; the library exists so `cargo test -p bench` can assert the harness itself (statistics, JSON-schema stability, probe logic, CLI parsing, report comparison, tool resolution, budget verdicts).
It does not merely *record*: `src/budget.rs` carries the explicit latency budgets and turns a finished `Report` into one verdict per budgeted quantity, so a run is **judged** as well as measured (see § Latency budgets).
`bench` is a **pure driver/observer**: it never modifies the Rust runtime and never writes to the external repos.
Full suites are milestone-only (Part I §C); the default `--quick` path stays bounded.

## Shared conventions (binding — from `../CONTEXT.md`, the `tools/` node)
- **Runtime resolution — documented, overridable, no baked absolute paths:**
  - **Oracle (`sys`)** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud env error (exit 2).
  - **Rust (`rust`)** = `$BEAM_RT` if set, else the workspace-relative `../../target/{release,debug}/beam_rt` (resolved from `CARGO_MANIFEST_DIR`).
  - Both are invoked with the **same argv as `erl`** — the runtime binary is treated as a drop-in `erl` (contract A.6). Only the *program path* is set; internals are never assumed.
- **Honest reporting:** a case/workload that cannot run is `skipped(<reason>)` / `failed(<reason>)` — **never** `pass`, and metrics are never fabricated. Unknown/unavailable ⇒ explicit reason string.
- **Machine-readable results:** JSON with a stable schema (below), written under the gitignored `out/` dir (already ignored by `../../.gitignore`: `/tools/bench/out/`) unless `--out` overrides. Durable, reviewable milestone results go to the **tracked** `results/` directory (see `RESULTS.md`).
- **No heavy new deps:** `serde`/`serde_json` (already workspace deps) for JSON; **hand-rolled `std`-only arg parsing** (no `clap`). `beam_common` (already a dep) may be used only for its logging facade.
- Files target ~1000 lines; split by responsibility. Apache-2.0. `unsafe` is **forbidden** (`#![forbid(unsafe_code)]`).
- External repos are **read-only** (`/home/bill/Source/{elixir,cowboy,khepri,emqx,Programming-Language-Benchmarks}`); anything that must build/run is staged into `$TMPDIR` first.

### Read-only target adapter (`src/scratch.rs`)
A reusable strategy so workloads never mutate the (read-only) source repos: `ScratchDir` is an RAII temp dir under `std::env::temp_dir()` (unique name, auto-removed on `Drop`); `copy_tree(src, dst, skip_names)` recursively copies a repo subtree (skipping named entries like `.git`/`_rel`/`deps` at every depth, preserving modes); `copy_files(files, dst)` copies an explicit file list.
Workloads stage the repo into a `ScratchDir`, build/run **there**, and let the `Drop` clean up; `--repo`/`$BENCH_<ID>_REPO` still select the *source* tree.
Verified: source repos stay untouched (`git status` clean) after a run.

### External-tool resolution (`src/workloads/tool.rs`) — no `PATH` prefix, no baked store paths
The tool-driven workloads (`khepri`, `emqx`, `plb`) resolve their external tool (`rebar3`, `mix`/`make`, `dotnet`) through one shared, documented policy, implemented once:
1. **`PATH`** (`proc::which`) — always consulted live;
2. **`$BENCH_TOOL_PATH`** — colon-separated extra `bin` dirs, empty entries dropped (the developer's override);
3. an **automatic Nix-store scan** for `/nix/store/*<tool>*/bin/<tool>`, ranked **by the version named in the entry** (descending) — the random 32-char hash prefix never participates, so the "newest installed" preference is real and a too-old version cannot win by accident; **no store hash or version is baked in**;
4. **`$BENCH_TRY_NIX`** (opt-in, default off): a validated `nix shell nixpkgs#<attr> --command <tool>` shim — last, consulted **only** when steps 1–3 find nothing.
Steps 1–3 are subprocess-free and memoised in a process-global cache, so `probe()` / `bench list` stay cheap; a `None` means the tool is genuinely unavailable ⇒ the caller keeps its honest `skipped(<tool>-absent)`.
`available()` (the `probe()` predicate) returns true when steps 1–3 resolve the tool **or** `$BENCH_TRY_NIX` is set, so `probe_ready` can read true while a later `--version` validation fails; the workload still reports honestly and never fabricates a number.

**Two resolution entry points.** `command_for(tool, nix_attr)` picks the **first** candidate in source order (steps 1–4) — the default for tools with no version constraint.
`command_for_min(tool, nix_attr, min_version: &[u64])` is the **version-aware** path for a tool whose consumer needs a minimum version (used by `plb`, which declares `MIN_DOTNET_SDK = &[9]` because PLB's envs target `net9`): it enumerates **every** candidate across `PATH` + `$BENCH_TOOL_PATH` + the store, probes each one's `--version`, and **selects the greatest version that satisfies `min_version`** — source order is only the tie-break, so a too-old `dotnet` on `PATH` can no longer shadow a newer store SDK.
A candidate whose version cannot be probed never satisfies; when nothing satisfies it still returns the best available candidate plus an explicit `no <tool> >= X available; using the best available …` note (never a silent wrong pick).
It returns a `ToolCommand { command, notes }`: the resolved binary + version + source and every rejection reason (`rejected dotnet 8.0.425 at <path> (<source>): requires >= 9`) go into the workload row's `notes`, in both the success and the failure branch, so the recorded outcome is auditable.
**Flake NOTE (do NOT edit `../../flake.nix`):** it lists `rebar3` in `oracleDeps` but no .NET SDK; adding one there would make `plb` reproducible.
The committed record of what each tool-driven target actually does here is `results/m19-target-notes.md` (activation) and `results/m19-ext-notes.md` (the three-rep external stage); host toolchain state is in Known Issues.

### `beam_rt` is a REAL runtime (self-checked)
`beam_rt` is a working Rust runtime (real `-eval` evaluator, a global-atom `code:*`/`init:*` floor, native `os:cmd/1`); it is resolved from `$BEAM_RT` or the workspace-relative `../../target/{release,debug}/beam_rt`.
A runtime is *supported* only if it can execute the canonical probe `-noshell -eval 'io:format("~p~n",[1+1]), halt().'` and print `2`.
A resolved, probe-passing runtime reports `supported: true` (the reported version comes from `erlang:system_info(otp_release)`; `beam_rt` prints `30`), so a workload scheduled under `rust` **runs**; an unresolved or probe-failing runtime is honestly `skipped(runtime-unsupported)`, **never** a fake number.

### Latency budgets (`src/budget.rs`) — a run is judged, not just recorded
`ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I and `ERTS_RUST_REWRITE_DEEPDIVE.md` mandate **startup and p99 tail-latency budgets** but name no numbers, and a driver that only records can never say whether a figure is acceptable; `budget.rs` supplies that half.
`BUDGETS` is a hand-pinned `const` table of **11** rows, each with a `workload`, a `runtime: Option<RuntimeKind>` (`None` = every runtime present in the report), a `metric` (exactly as the workload records it), a `stat` (`Value`/`P50`/`P99`), a `limit` (`AtMost` absolute or `RatioAtMost` = rust/sys) and a one-sentence `rationale` naming what the limit is **not** (a regression alarm that passes today — never an SLO for this shared host, never a parity target).
The budgeted quantities are the mandated startup/p99 budgets plus one first-pass regression alarm per cross-runtime class: `baseline` rust `startup_ms` p50 and p99, each absolute *and* as a rust/sys ratio; `baseline` sys `startup_ms` p99 (the host-artifact attribution row, so a load spike cannot be misread as a Rust regression); the `cowboy[sys]` request tail (the row's real metric is `latency_ms`, read through its `p99` summary — there is no separate p99 metric); and one per-unit alarm each for `list_walk`, `copy_bound`, `gc_bound`, `file_io` and `gc_class`.
The limits, their units and the figures each was tuned against live **only** in `src/budget.rs` — read them there (this file restates no limit).
Verdicts are `Met`/`Exceeded`/`NotMeasured { reason }`, and `evaluate(&Report)` is **pure** (no I/O, no spawn, no clock), so an archived report — even one written by an older driver — can be judged offline.
`NotMeasured` is honest silence with a **specific** reason: workload absent (not registered / not selected by `--workloads`), no row for that runtime (not selected, or unsupported here), a `skipped`/`failed` row (quoting its reason), a missing metric (quoting the row note when there is one), a metric of the wrong shape for the stat, a non-finite figure, or a `RatioAtMost` row with no usable `sys` denominator.
A ratio row needs **both sides in one report** — a `rust`-only report leaves every ratio row `NotMeasured` — and a ratio row declared on `sys` is refused as a self-comparison; bounds are inclusive (`measured <= limit` is `Met`, ratios alike).
`Report` carries the verdicts as `budgets: Vec<BudgetVerdict>` (defaulted on load), each repeating its `id`/`limit`/`rationale` so the saved report is self-describing.
`SCHEMA_VERSION` **stays 1**: the field is purely additive, a pre-budget report loads and compares unchanged, and re-judging can only *add* verdicts — it can never produce a wrong number, the only thing a version bump would buy.
`run`/`baseline` evaluate the budgets once the result set is final, store them, and print a compact `latency budgets — N met / N exceeded / N not measured` block (one line per budget) on **stderr**, keeping stdout clean; `--fail-on-budget` makes ≥ 1 `Exceeded` exit `1`, while a `NotMeasured` budget can never fail a run.

## API Surface

### CLI (hand-rolled; usable `--help` on every subcommand)
- `bench list [--json]` — list workload ids, category, contract coverage, and current probe status.
- `bench run [--quick|--full] [--workloads <ids,csv>] [--runtime <sys|rust|all>] [--out <dir>] [--out-file <path>] [--repo <id>=<path>] [--json] [--verbose] [--fail-on-budget]`
- `bench baseline [--quick|--full] [--json] [--out <dir>] [--out-file <path>] [--fail-on-budget]` — measure **only** the system-ERTS baseline.
- `bench compare <report.json> [--json]` — **cross-runtime**: diff the `sys` rows against the `rust` rows of one report (as produced by `bench run --runtime all`).
- `bench compare <a.json> <b.json> [--json]` — **two-file**: diff two result sets (`a` = baseline).
- `bench help [<subcommand>]` — top-level or per-subcommand help.

`--fail-on-budget` exists on `run` and `baseline` only (it is a usage error on `list`/`compare`, which judge nothing).
Defaults: `run` ⇒ `--quick`, `--runtime sys`, all workloads. Report file = `<out>/bench-<profile>-<unix_ms>.json` (`<out>` = the crate's `out/` dir), or the **exact** path given by `--out-file <path>` when set.
Exit codes: `0` = report produced (per-workload `skipped(reason)` and `failed` are recorded in JSON, not fatal); `1` = a budget was exceeded **and** `--fail-on-budget` was given; `2` = usage/env error (bad flag/arg, or the oracle cannot be resolved for `run`/`baseline`). `compare`: `0` = compared, `2` = usage/IO/schema error. A workload that *errors* at runtime is recorded `failed` in JSON (does not change the exit code).

### Workload plugin trait (library)
```rust
pub enum Probe { Ready, Skipped(String) }          // reason is a short kebab string, e.g. "rebar3-absent"
pub enum RuntimeKind { Sys, Rust }
pub enum Runtimes { Both, SysOnly }                // which runtimes a workload may be scheduled under
pub enum Category { Startup, Compiler, Compute, Gc, Io, Messaging }
pub struct RunContext<'a> { pub runtime: &'a Runtime, pub profile: Profile, pub out_dir: &'a Path }
pub trait Workload {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn category(&self) -> Category;
    fn contracts(&self) -> &'static [&'static str]; // e.g. ["A.1","A.9"]
    fn runtimes(&self) -> Runtimes;
    fn probe(&self) -> Probe;                       // honest prerequisite check
    fn run(&self, ctx: &RunContext) -> WorkloadResult;
}
pub fn registry() -> Vec<Box<dyn Workload>>;
pub fn find(id: &str) -> Option<Box<dyn Workload>>;
```
Runner rule: every selected **(workload × runtime)** pair yields **exactly one** `WorkloadResult`.
`runtimes()` gates scheduling; a workload scheduled under a runtime it does not support ⇒ `Skipped("runtime-unsupported")`; a `Probe::Skipped(reason)` ⇒ `Skipped(reason)` without running.

### Built-in workloads — 15 (9 `Runtimes::Both`, 6 `SysOnly`)
| id | category | target repo / tool | today | skip reason(s) |
|---|---|---|---|---|
| `baseline` | startup | oracle `erl` | **runs** (sys+rust) | `runtime-unsupported` |
| `erlc_compiler` | compiler | `erlc` over OTP stdlib **+** Elixir `lib/elixir/src` `.erl` sources | **runs** (sys only, quick+full) | `erlc-absent`, `corpus-missing` |
| `elixir_compiler` | compiler | system `elixirc` over a staged `.ex` corpus from the Elixir checkout | **runs** (sys only, quick+full) | `elixirc-absent`, `corpus-missing`, `repo-missing` |
| `plb` | compute | `/home/bill/Source/Programming-Language-Benchmarks` (dotnet tool, two-stage build+bench; needs a ≥ 9.0 SDK, resolved by `command_for_min`) | **runs** (sys) | `dotnet-absent`, `repo-missing`, `plb-build-failed`, `plb-tool-failed`, `plb-no-results` |
| `cowboy` | io | `/home/bill/Source/cowboy` hello_world built in a scratch copy, driven by the in-process loader | **runs** (sys) | `repo-missing`, `make-absent`, `deps-unavailable` |
| `khepri` | gc | `/home/bill/Source/khepri` (`rebar3 eunit`, in a scratch copy) | **runs** (sys) | `rebar3-absent`, `repo-missing` |
| `emqx` | messaging | `/home/bill/Source/emqx` (real `make`/`mix` build, in a scratch copy) | `skipped(emqx-requires-otp-28)` on this OTP-29 host (checkout pins OTP **28.5.0.7-2**; see Known Issues) | `mix-absent`, `make-absent`, `repo-missing`, `not-an-emqx-project`, `emqx-requires-otp-28`, `emqx-build-failed` |
| `slice` | compute | committed `slice.beam` fixture + stdlib wrapper modules, run under **each scheduled runtime** | **runs** (sys+rust) | `erlc-absent`, `fixture-missing`, `unsupported-case-set`, `runtime-unsupported` |
| `foreign` | compute | **real Cowboy + EMQX modules** (`cowboy_bstr.erl`, `emqx_base62.erl`) compiled with the system `erlc`, run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `sources-missing`, `runtime-unsupported` |
| `list_walk` | compute | in-tree embedded `bench_list_walk` module (BIF-free recursive `sum/1` list walk), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `unsupported-case-set`, `runtime-unsupported` |
| `copy_bound` | messaging | in-tree embedded `bench_copy_bound` module (ETF / ETS / two-process copy round-trips, with and without the codec in the receiver), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `module-compile-failed`, `runtime-unsupported` |
| `gc_bound` | gc | in-tree embedded `bench_gc_bound` module (allocate-and-abandon garbage churn), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `unsupported-case`, `runtime-unsupported` |
| `gc_class` | gc | the **sibling fixtures** `gc_class_copy_bound.erl`/`gc_class_compile.erl` (copied read-only into a `ScratchDir`) + a bench-owned driver, run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `fixtures-missing`, `compile-source-missing`, `unsupported-case-set`, `runtime-unsupported` |
| `file_io` | io | in-tree embedded `bench_file_io` module (write/read round trips in a scratch dir), run under **both** runtimes | **runs** (sys+rust); `ram_file` is absent on rust | `erlc-absent`, `module-compile-failed`, `unsupported-case-set`, `runtime-unsupported` |
| `binary_append` | compute | in-tree embedded `bench_binary_append` module (16 KiB → 64 KiB byte-quadrupling `<<B/binary,…>>` comprehension = 16 384 appends into one writable payload), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `module-compile-failed`, `unsupported-case-set`, `runtime-unsupported` |

### Per-workload detail
- `baseline` (always-on): N runs of `<erl> -noshell -eval 'halt().'` ⇒ the `startup_ms` `Summary` (p50/p99); a fixed in-VM compute loop (`lists:sum(lists:seq(1,1000000))`) ⇒ `compute_loop_ms` + `compute_ops_per_sec`; and a **separate** `erlang:memory(total)` probe ⇒ the scalar `vm_memory_total_bytes`, never conflated with the OS `peak_memory_bytes` (the compute runs' RSS).
  `--quick` ⇒ 10 startup runs / 5 compute runs, `--full` ⇒ 50 / 5; both runtimes produce all three metrics (`beam_rt` serves `erlang:memory/0,1` through the `MemoryHook` seam).
  A capability gap **degrades instead of aborting** — no metric plus an explicit `… : not-run (unsupported: …)` note while every other metric is still produced — while a genuine (non-capability-gap) failure is surfaced verbatim.
  The loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1`, so a near-parity rust figure is the native floor, **not** execution throughput.
- `erlc_compiler` / `elixir_compiler` — the two `SysOnly` compiler rows, both recording the compiler they actually drove as `code:which(compile)=<path>` (see the compile-attribution guard).
  `erlc_compiler` compiles a bounded deterministic slice (first 30 `.erl` of each corpus) of the in-repo OTP `lib/stdlib/src` **and** the Elixir checkout's `lib/elixir/src` into a `ScratchDir` ⇒ files/sec, LOC/sec, wall time, peak memory (a missing/failing Elixir corpus falls back to OTP-only with a note).
  `elixir_compiler` measures the **installed system `elixirc`** over a fixed `.ex` corpus staged into a `ScratchDir` ⇒ wall time, files/sec, LOC/sec, compiler-startup, peak memory, one batch run falling back to per-file compilation (the checkout↔installed version skew) with a note saying which mode ran.
- `cowboy`: `copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk git-fetches cowlib/ranch), starts the release `foreground` on port 8080, drives it with `http_load::drive` (in-process keep-alive HTTP/1.1), records build/latency/throughput and the server's peak RSS, then stops it.
  Build/dep failure ⇒ `skipped(deps-unavailable)`, compile error ⇒ `failed`; its latency metric is `latency_ms` (a `Summary`, whose `p99` the request-tail budget reads).
- `khepri`: stages the checkout into a `ScratchDir` and runs `rebar3 eunit` there ⇒ wall time + peak memory (`rebar3` via the store scan; absent ⇒ `skipped(rebar3-absent)`), recording the resolved `rebar3` **and the Erlang the suite actually ran under** (`shebang_interpreter`, read from the escript's head, lossily decoded, `None` for a compiled `rebar3`) — which need not be the driver's oracle.
- `emqx`: stages the checkout and drives EMQX's **real** build entry point (`make`/`mix` umbrella: `mix deps.get` + `mix compile` via the repo's own `./mix`) in the scratch copy, with `HOME`/`MIX_HOME`/`HEX_HOME` redirected into the scratch.
  A mismatched host records the **precise** blocker rather than a fabricated number (see Known Issues); the gate reads **only** the checkout's declared OTP pin, while `absent_build_tools()` probes `automake`/`libtool`/`cmake` per run so installing one changes the note instead of leaving a stale claim.
  `mix`/`make` absent ⇒ `mix-absent`/`make-absent`; not an EMQX checkout ⇒ `not-an-emqx-project`.
- `plb`: stages the checkout into a `ScratchDir` and drives the dotnet tool in **two stages** — `dotnet run -c Release --project tool -- --task build --langs elixir --no-docker` (copies sources + templates, runs the env's `build:` command, moves the result under `bench/build/<build-id>`) then `--task bench --langs elixir` — reading `bench/build/_results/**/*.json` (`timeMS`/`memBytes`) from the **scratch** copy.
  `elixir` is the only language driven (no working container runtime) and the row records `langs=elixir` so the coverage is auditable; `HOME`/`DOTNET_CLI_HOME`/`NUGET_PACKAGES`/`MIX_HOME`/`TMPDIR` are redirected into the scratch, and `tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` writes the chosen binary + version + source (and every rejection) into the row's notes.
  **A `timeMS` of `0` is an absence, not an instant:** the tool writes a result JSON even for a run aborted at its declared timeout, recording `Elapsed = 0` while still reporting consumed CPU, so those results are **excluded** from the aggregate and each is named in a note.
  Honesty: no result JSON ⇒ `failed(plb-tool-failed)`, nothing timed ⇒ `failed(plb-no-results)`, a broken build stage ⇒ `failed(plb-build-failed)` — **never** `ok`.
- `slice`: runs a fixed table of **module-run cases** under whatever runtime it is scheduled on (so under `--runtime all` it is a real sys-vs-rust comparison of the *same* programs): 3 cases use the committed `slice.beam` fixture (`add/2`, `fib/1`, `pick/1`) and the rest are tiny embedded stdlib/kernel wrappers calling a *real* system module (`calendar`/`queue`/`gb_trees`/`base64`/`ordsets`/`string`), compiled once with the system `erlc`.
  `--quick` ⇒ 5 timed runs/case, `--full` ⇒ 25; metrics are per-case summaries `case_<name>_wall_ms`/`case_<name>_peak_rss_bytes` (external `VmHWM`), an `observed stdout:` note records the agreement, and a Rust exit `3` yields a note and **no** timing.
  A `beam_rt` process resolves the system code path with a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`; the `erl` shell-out survives only as a last resort), so no case pays an `erl` startup.
  `slice` is the only loop-free workload (`N`=1), so it takes no marginal: its figures are whole-process wall times and its peak RSS is the one like-for-like memory comparison.
- The seven loop workloads below (`foreign`, `list_walk`, `copy_bound`, `gc_bound`, `gc_class`, `file_io`, `binary_append`) share one recipe: a module whose Erlang source is **generated from the crate's Rust constants** (so the two sides cannot drift) and compiled once with the system `erlc`, run **identically** under each scheduled runtime through `beam_rt`'s `run` action, sampled as **interleaved pairs** (`N` then `0`, back to back) and published as `case_<name>_{wall_ms,startup_ms,peak_rss_bytes,ns_per_*}` plus the deterministic `case_<name>_ops`/`_calls`/`_elements` companion (the contention-immune reading); `--quick` ⇒ 3 rounds, `--full` ⇒ 10.
  Every row checks its runtime's stdout against the deterministic constant (an `observed stdout:` note); a Rust exit `3` records the capability gap and emits **no** timing; an embedded-source compile failure is `failed(module-compile-failed)` (the source is ours, never a skip); `probe()` ⇒ `skipped(erlc-absent)` via the shared `slice::resolve_erlc()`, and a missing/unsupported runtime is `skipped(runtime-unsupported)`.
- `foreign`: **real modules from the foreign target repos, run under *both* runtimes.** Every repo-backed workload is `SysOnly`, so `foreign` instead picks pure, self-contained modules from the Cowboy and EMQX checkouts (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`) and runs the identical `.beam` under whatever runtime it is scheduled on, reporting `case_<name>_ns_per_call` (+ `case_<name>_calls` = `N`).
  Its `Case` supplies the driver, the per-call increment and the per-runtime `N`; `foreign_sample.rs` runs the two passes interleaved, checks the deterministic stdout, classifies failures (a Rust exit `3` is `unsupported` only while no work sample has completed) and hands the paired wall times to `crate::marginal`.
  The Rust row notes that `ns_per_call` is **N-dependent**, and `foreign` peak RSS is **not** a like-for-like memory comparison (see Known Issues).
- `list_walk`: the permanent, reproducible **per-element dispatch-throughput** signal — `bench_list_walk`'s `sum/1` is a **BIF-free, non-tail-recursive** walk over a fixed-length list built by a pure-Erlang `build/2` recursion (no native `lists:seq/2`), ~7 dispatched instructions per element, exercising only the engine's dispatch/return path (the baseline tier does not inline it).
  `walk(N)` returns the deterministic `N × (LEN×(LEN+1)/2)` with `LEN = 1000` (a Rust const formatted into the Erlang source), and the row reports `case_list_walk_ns_per_element` + `case_list_walk_elements` = `N × LEN` (valid even on a loaded host).
- `copy_bound`: the **messaging / term-copy-bound** metric — four cases copying the **same** structured payload, the **one** compile-time literal `payload() -> #{a => [1,2,3,4,5,6,7,8,9,10], b => {ok, <<"hello world">>, 3.5}, c => <<1,2,3,4,5,6,7,8,9,10,11,12>>, d => "a char list", e => -42}` (**70 words / 27 objects / 21 cons cells** per `../../crates/beam_message/PERF.md`'s census), each validated by a bound-variable pattern (a real equality test that raises on a corrupted copy).
  The cases are `etf_roundtrip` (`term_to_binary/1` + `binary_to_term/1`, contract A.5), `ets_copy` (`ets:insert/2` + `ets:lookup/2`, one table+key per process so the measured work is the copy, not table growth), `send_recv` (a second process round-trips the term through `term_to_binary/1` — **both** directions of the process copy boundary, with an `after` guard so a missing reply fails loudly, never hangs) and `send_recv_plain` (the identical two-process shape replying the payload *directly*, so `send_recv − send_recv_plain` attributes the codec component *inside* the two-process path).
  **Attribution caveat:** `payload/0` is rebuilt and the equality match re-run inside every iteration, so a per-op figure is the *whole iteration*, **not the codec alone** — the same holds for the other three cases, and both runtimes pay the identical setup.
- `gc_bound`: the **GC-bound allocation** metric — one embedded `bench_gc_bound` module whose `churn/1` case builds and abandons three `LEN`=256-sized structures per iteration (a pure-Erlang recursion list via `build_list/2`, a 256-byte binary built from an Erlang-level char list, and a tuple built from another Erlang list) and then checks the two O(1)-checkable ones (`byte_size`/`tuple_size`) so a silently-dropped allocation fails loudly.
  It reports `case_churn_ns_per_op` and **claims no collection count** (whether a collector runs during the loop is each runtime's own admission policy); a Rust exit `3` ⇒ `skipped(unsupported-case)`.
- `gc_class`: **the GC-bound workload *classes*** — the shapes the GC-strategy A/B harness (`crates/beam_rt/tests/gc_strategy_classes.rs`) measures, as opposed to `gc_bound`'s single allocation loop.
  It reuses that harness's **committed fixtures read-only** (`crates/beam_rt/tests/fixtures/gc_class_copy_bound.erl` and `gc_class_compile.erl`, copied into the `ScratchDir` at run time and never edited here) compiled with the system `erlc` together with a bench-owned driver (`bench_gc_class_driver`) that only drives them and validates what comes back.
  Three cases: `send_recv` and `ets_copy` (the process and ETS copy boundaries) and `compile` (`gc_class_compile:run/1`, a real **in-process** compile of `lib/stdlib/src/ordsets.erl`, whose inputs arrive through the child environment `BEAM_RT_GCH_SRC`/`BEAM_RT_GCH_OUT` and whose driver pre-creates `out1..out<n>` because `compile:file/2` does not create its `{outdir, Dir}`).
  The payload is `{K, [{K, I, I + K} || I <- 1..64]}` — ~390 words, deliberately **above the default 233-word nursery** and rebuilt every round — so a round asks a heap-admission door for more than a whole nursery and a collection really is admitted; that is what makes these rows a *GC* measurement rather than another copy measurement (and why the payload differs from `copy_bound`'s 70-word literal wrapper).
  A missing fixture source ⇒ `skipped(fixtures-missing)`; a missing `ordsets.erl` ⇒ `skipped(compile-source-missing)`; a non-compiling fixture/driver ⇒ `failed(module-compile-failed)`.
- `file_io`: **the I/O-bound class** — file-system round trips under *both* runtimes (the only *both*-runtime I/O row; `cowboy` is `SysOnly`), so the only thing that can differ between the two rows is how each runtime services the file-system calls.
  One embedded `bench_file_io` module generated from the crate's constants — a **4 096-byte** payload (`PAYLOAD_BYTES`), a per-op increment of 1 (`PER_OP`) — with three cases: `write_read` (`file:write_file/2` + `file:read_file/1`, the whole-file view through OTP's `file_server`), `handle_rw` (`[write,raw,binary]` open + write + close, then `[read,raw,binary]` open + `file:read/2` + close, the raw-handle view), and the **optional** `ram_file` (`[ram,read,write,binary]` + write + reposition + read + close, the `ram_file_drv` path).
  Every iteration matches the read-back bytes against the written payload exactly, so a lost or truncated write fails the run loudly; the payload is built once per process, outside the timed loop, by interpreted recursion.
  The scratch path reaches the module through the **child environment** (`BENCH_FILE_IO_DIR`, read with `os:getenv/1`) because the `run` action carries only integers, and an unset variable is a loud `{missing_env, …}` exit rather than a write into the current directory.
  `ram_file` **verifies itself** before the loop (open, write, reposition, read back, close, then assert the name is gone — `{error, enoent}`) because a runtime that silently ignored the `ram` option would materialise a host file and pass an ordinary disk figure off as a RAM one.
  **`ram_file` is absent on the rust side** — `beam_rt` cannot serve a `ram` file (it execs `ram_file_drv`, which is not there) — so the case carries no rust metric and its figure is a **sys-only** number, never a fabricated pairing; an optional case's absence is never a capability gap, and a *required* case's failure is never downgraded to absent.
- `binary_append`: the **binary-append (writable-accumulator)** metric — the in-place growth path under *both* runtimes.
  One embedded `bench_binary_append` module (`INPUT_ARG` = 8 KiB ⇒ 16 KiB in, `OUT_BYTES_PER_CALL` = 65 536 out) whose `expand/1` is the compiler's writable-accumulator squeeze `<< <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>` — 16 384 four-byte segments appended into **one** writable payload — called `N` times by `run(N)`, which returns the deterministic `N × 65 536`.
  Metrics: `case_binary_append_{wall_ms,startup_ms,peak_rss_bytes,ns_per_call}` (the shared paired median, withheld in the noise band) plus `case_binary_append_calls` = `N` and `case_binary_append_bytes` = `N × 65 536`.
  The row measures the **append path**, not a series of copies, and the deterministic stdout proves neither side dropped an append; `n_rust` is capped by `beam_rt`'s non-overridable 60 s `run` deadline (see Constraints), which is why the interpreter tier reports `failed(no-binary-append-run)` instead of a number.

### The compile-attribution guard — `code:which(compile)`
A wrong `-pa` (or an inherited `ERL_FLAGS=-pa <system ebin>`) makes an in-process compile be served by a **different** compiler, which moves every compile figure by roughly a factor of two.
Every compile row therefore records `code:which(compile)=<path>` in its notes: `gc_class`'s `compile` case prints it **from the compiling process itself** (the driver emits `which=<path>` as its own stdout line — a missing line is recorded as a note, and the 0-op startup pass compiles nothing so it legitimately prints none), and `erlc_compiler`/`elixir_compiler` probe the compiler they actually drive with the same `-eval` expression (in the node that would run it, inheriting `ERL_FLAGS`/`ERL_COMPILER_OPTIONS` exactly as `erlc` does).
`compiler_origin` classifies the answer (this tree's `bootstrap/lib/compiler/ebin` / elsewhere in this checkout / the **system OTP** installation / unreadable), and on the `rust` side a path resolving into the system OTP's `lib/compiler-*/ebin` additionally emits a loud `WARNING:` note so the figure can never be quoted without its attribution.
**On this host both sides resolve `compile` to the system OTP 29.1.1 compiler** (`…-erlang-29.1.1/lib/erlang/lib/compiler-10.0.6/ebin/compile.beam`), never this tree's `bootstrap/lib/compiler/ebin`.
So `gc_class`'s compile figures are a **runtime-execution** comparison at a *controlled* compiler — how fast each runtime runs the same work — **not** an in-tree/bootstrap-compiler measurement.

### No user-supplied module / compiler flags — the sanctioned knobs are env-only
- **There is no CLI or config route to hand the driver your own module, your own compile-option set, or your own loop count.** The workloads are a fixed Rust registry (`src/workloads/mod.rs:121-139`, ids pinned by the `registry_has_all_documented_workloads` test), selected only by id (`--workloads`), and every compile site hardcodes `erlc -o <dir> <src>` with no extra flags (`src/workloads/slice.rs::compile_module`, `src/workloads/list_walk.rs::stage`). `N`/`LEN`/the case tables are compile-time constants.
- **The only compiler knobs are the child environment** (inherited by every spawn): `$ERLC` (a *file*, path-only — `src/workloads/slice.rs::resolve_erlc`) and `ERL_COMPILER_OPTIONS`, which `compile:file/2` appends to its options (`lib/compiler/src/compile.erl::env_default_opts/0`; the erlc path is `compile:compile/3` → `file/2`).
  `ERL_FLAGS="-pa <ebin>"` is the documented way to put a *different* `compile` module on the erlc/erl code path (`erts/doc/references/erl_cmd.md` §`ERL_FLAGS`; `erts/etc/common/erlc.c` forwards it, `lib/kernel/src/erl_compile_server.erl` runs the compiler in that node); `erlc +Opt` is the equivalent command-line form (`lib/stdlib/src/erl_compile.erl`).
- So a two-compiler A/B is: two `bench run --runtime sys --workloads <module-workload>` passes in two environments, then `bench compare a.json b.json`, over several alternating pinned rounds (each report's marginal cancels noise only *within* its own run). An *arbitrary* module needs either a new `Workload` impl or the crate used as a library (`bench::marginal::{sample_pairs, resolve}`, `bench::proc::run` — `src/lib.rs`).

### Result JSON schema (stable — `SCHEMA_VERSION = 1`)
Written by every `run`/`baseline`; consumed by `compare`. Durations/latencies in **milliseconds** (f64).
```jsonc
{
  "schema_version": 1, "tool": "bench", "tool_version": "0.1.0",
  "generated_at_unix_ms": 1730000000000, "profile": "quick",   // "quick" | "full"
  "runtimes": [
    { "kind": "sys",  "program": "/nix/.../erl", "version": "29.1.1", "supported": true,  "notes": [] },
    { "kind": "rust", "program": ".../target/release/beam_rt", "version": "30", "supported": true, "notes": [] }
  ],
  "budgets": [                              // defaulted on load: a pre-budget report has no key
    { "verdict": "met",                     // "met" | "exceeded" | "not_measured"
      "id": { "workload": "baseline", "runtime": "rust", "metric": "startup_ms", "stat": "p50" },
      "limit": { "at_most": 800.0 },        // or { "ratio_at_most": 6.0 }
      "measured": 260.16,                   // absent for "not_measured"
      "rationale": "…",                     // the row's own sentence, so the verdict stands alone
      "reason": "…" },                      // "not_measured" only: which row/metric is missing, and why
  ],
  "results": [
    { "workload": "baseline", "runtime": "sys", "category": "startup",
      "description": "system-ERTS startup latency + fixed compute loop",
      "status": "ok",                       // "ok" | "skipped" | "failed"
      "reason": null,                       // skip reason when status == "skipped"
      "metrics": [
        { "name": "startup_ms", "unit": "ms", "summary": { "count": 10, "min": 108.0, "max": 210.0, "mean": 131.4, "p50": 124.0, "p90": 180.0, "p99": 205.0 } },
        { "name": "compute_ops_per_sec", "unit": "ops/s", "value": 10870000.0 }
      ],
      "peak_memory_bytes": 34000000,        // null when unmeasurable (see Constraints)
      "notes": [] }
  ]
}
```
`Metric` = `{ name, unit, value? | summary? }` (exactly one of `value`/`summary`); `Summary` = `{ count, min, max, mean, p50, p90, p99 }` (all f64 except `count: usize`).
`BudgetVerdict` is tagged by `verdict`; `BudgetId` = `{ workload, runtime, metric, stat }` and `Limit` = `{ at_most } | { ratio_at_most }`.

## Library modules (routing table)
- `src/lib.rs` → crate docs, module wiring, `#![forbid(unsafe_code)]`, root re-exports (including `budget::{Budget, BudgetId, BudgetVerdict, Limit, Stat, BUDGETS}`).
- `src/main.rs` → thin binary entry: parse args, dispatch, map result to exit code.
- `src/cli.rs` → hand-rolled arg parser, `Command` enum (incl. `--fail-on-budget` on `run`/`baseline`), usage/help text, `UsageError` (incl. `--out-file`).
- `src/error.rs` → crate error/`Result` type (`BenchError`: usage/env/IO/serde), `From<io::Error>`.
- `src/stats.rs` → `Summary`, `summarize(&[f64]) -> Summary`, `percentile(sorted, p)`, `mean`.
- `src/metrics.rs` → `Metric`, `Metric::scalar`/`Metric::samples`, `Status`, `Category`, `Probe`, `Runtimes`.
- `src/runtime.rs` → `RuntimeKind`, `Runtime`, oracle/rust resolution, `self_check`, `RuntimeInfo`.
- `src/proc.rs` → subprocess helpers: timed run with stdout/stderr capture, `VmHWM` peak-memory poller.
- `src/scratch.rs` → **read-only target adapter**: `ScratchDir` (RAII `$TMPDIR` temp dir), `copy_tree`, `copy_files`.
- `src/http_load.rs` → dependency-free in-process HTTP/1.1 load driver (`LoadConfig`, `drive`, `LoadOutcome`).
- `src/marginal.rs` → **the paired, drift-cancelling startup-cancelled marginal**: `Pass`/`PassError`/`Abort`/`PairedRuns`, `sample_pairs(runs, sample)` (Work-then-Startup **within** every round), `resolve(&[(work_ms, startup_ms)], units) -> Marginal` (median of the paired deltas, `1.4826 × MAD` noise band floored at `MIN_NOISE_MS`, published only above `NOISE_MULTIPLE`), `Marginal::{unit_cost_ns, detail, below_resolution_note}`, and the drift pin `tests::interleaving_cancels_a_drifting_boot_cost`. The sanctioned schedule for every per-unit metric.
- `src/budget.rs` → **the latency-budget mechanism**: `Stat` (`Value`/`P50`/`P99`) + `extract`, `Limit` (`AtMost`/`RatioAtMost`) + `value`/`is_ratio`/`describe`, `Budget`, `BudgetId` (`Display` → `baseline[rust] startup_ms p50`), `BudgetVerdict` (`Met`/`Exceeded`/`NotMeasured` with `id`/`limit`/`rationale`/`measured`/`reason`/`is_exceeded`), the pinned `BUDGETS` table, `evaluate(&Report)` (**pure**), `exceeded_count`, `any_exceeded`, `render_block` (the stderr tally) and `render_line`.
- `src/report.rs` → `Report` (incl. the defaulted `budgets` section), `WorkloadResult`, `SCHEMA_VERSION`, JSON save/load (serde) + `validate`.
- `src/compare.rs` → diff two `Report`s into per-(workload,runtime,metric) deltas; one-path mode diffs `sys` vs `rust` within a single report. The budget section never disturbs a comparison.
- `src/run.rs` → runner: resolve runtimes, iterate (runtime × workload), assemble `Report`, evaluate + store the budget verdicts, write JSON, print the stderr budget block, map a breach to exit `1` under `--fail-on-budget`.
- `src/workload.rs` → the `Workload` trait, `RunContext`, `registry()`, `find()`.
- `src/workloads/mod.rs` → submodule wiring; assembles the 15-workload registry; shared repo/file helpers; the coverage pin `every_mandated_workload_class_has_a_cross_runtime_workload` (each of Startup/Compute/Messaging/Gc/Io must keep ≥ 1 `Runtimes::Both` workload, so deleting or demoting the only one fails by name instead of silently shrinking the suite).
- `src/workloads/tool.rs` → shared external-tool resolution (`available`, `command_for` (source order), `command_for_min` (version-aware: `Candidate`/`ToolCommand`/`select_candidate`/`probe_tool_version`), `extra_tool_dirs`, the Nix-store scan with **version-aware** ranking, `parse_tool_path`, `REPO_SKIP_NAMES`).
- `src/workloads/baseline.rs` · `erlc_compiler.rs` · `elixir_compiler.rs` · `plb.rs` · `cowboy.rs` · `khepri.rs` · `emqx.rs` · `slice.rs` · `foreign.rs` · `list_walk.rs` · `copy_bound.rs` · `gc_bound.rs` · `gc_class.rs` · `file_io.rs` · `binary_append.rs` → one file per workload (probe + run), carrying the loop-count calibration (`const CASES` tables + the `the_rust_loop_count(s)_clear_the_boot_jitter_band` tests).
  `slice.rs` also exposes `pub(crate) resolve_erlc()` (the shared `$ERLC`/`PATH` lookup reused by the seven loop workloads), and `gc_class.rs` the compile-attribution helpers `erl_compiler_which_note()` (used by `erlc_compiler`/`elixir_compiler`), `compiler_origin()` and `which_note()`.
- `src/workloads/foreign_sample.rs` → the `foreign` sampling machinery, extracted so `foreign.rs` stays inside the ~1000-line convention: `CaseOutcome`/`CaseSample`/`sample_once`/`outcome_from`/`classify_failure`/`execute_case`, all `pub(super)`.
- `tests/integration.rs` → hermetic synthetic-report round-trip + `compare` tests (no oracle needed), plus the cross-runtime regression guard: `foreign`, `list_walk`, `copy_bound`, `gc_bound`, `file_io` and `gc_class` stay `Runtimes::Both`, each with its own category/contracts (`foreign`/`list_walk` Compute·A.1+A.2, `copy_bound` Messaging·A.1+A.5, `gc_bound` Gc·A.1, `gc_class` Gc·A.1+A.2+A.5+A.9, `file_io` Io·A.1+A.7).
- `tests/budget.rs` → the budget mechanism against **real archived reports** (`results/m17-xab-{rust,sys}-r1.json`): a pre-budget report loads with an empty section and still compares, a merged matched round meets every budget it can measure, a rust-only report leaves the ratio rows `NotMeasured` with a reason, recorded verdicts survive save/reload, plus the coherence pins (`budget_table_is_coherent`, `every_mandated_quantity_has_a_row`, `an_absent_workload_is_not_measured_never_a_panic_or_failure`) and the render/JSON round-trips; a no-op skip when the archived files are absent.
- `tests/oracle_smoke.rs` → `baseline` smoke test against the real oracle (no-op skip when unresolved).
- `RESULTS.md` → the definitive results summary (per-category tables, both sides named, the matched-round methodology, the load record, the reproduction commands, the still-open items, honesty caveats). **Every measured number lives there, never here.**
- `results/` → committed raw `Report` JSON + supporting artifacts. **`m19-*` is the current session**: `m19-xab-{rust,sys}-r{1..6}.json` are the **twelve single-side matched passes** (six rounds, alternating side order, each pinned with `taskset -c 3`, all nine `Runtimes::Both` workloads) with `m19-xab-load.log` and `m19-xab-ratios.txt`; `m19-budgetprobe-all{,-cowboy}.{json,txt}` are the `--runtime all` budget-evaluation passes (the `.txt` being the stderr verdict block); `m19-full-baseline-{rust,sys}.json` is the `--full` startup pair; `m19-ext-<target>-r{1,2,3}.json` + `m19-ext-emqx.json` + `m19-ext-load.log` + `m19-ext-notes.md` are the three-rep external stage; `m19-target-{plb,khepri,emqx,ext}.json` + `m19-target-notes.md` are the **activation evidence**.
  History (one line): `m17-*`/`m16-*` are the superseded matched rounds, binary A/Bs and `m16-bench-xrun-interp.json` (still the interpreter-tier control); `m15-*`…`m4-*`, `b54-*`/`b55-*` are older regression baselines and the Θ(N³) `foreign` experiments, with the prose records `m1{3,4}-load-record.md`, `m1{2,3,4}-tool-targets-notes.md`, `m12-foreign-regression.md`, `m12-run-route-latency.md`, `m12-emqx-enablement-attempt.md` (+ `.patch`).

## Constraints
- `#![forbid(unsafe_code)]`; **no new crates** (deps resolve offline: `serde`, `serde_json`, `beam_common`).
- **Loop-count calibration (the rust marginal must clear the measurement noise).** Each `Runtimes::Both` loop workload declares a **per-runtime loop count `N`** and samples the identical program at `N` and at `0` as **interleaved pairs** (`src/marginal.rs`).
  The rust `N` is chosen so the timed loop does a uniform **≥0.5 s of loop work**: with the interleaving the residual noise is the MAD of the paired differences (a few milliseconds), which ≥0.5 s clears by orders of magnitude; `n_sys` stays far larger, so the oracle's own marginal is ≈1–1.2 s.
  Cross-`N` comparison is honest **only** through the per-op/per-element/per-call normalization — never compare raw `wall_ms` across two sides with different `N`; every row also emits the deterministic `case_*_ops`/`case_*_calls`/`case_*_elements` companion so the `N` behind each figure is in the report.
  Current counts (rust/sys): `list_walk` 50 000/100 000; `foreign` `bstr_to_lower` 50 000/1.5 M, `bstr_capitalize_token` 40 000/1.5 M, `emqx_base62_encode` 60 000/3 M, `emqx_base62_decode` 60 000/3 M; `copy_bound` `etf_roundtrip` 500 000/1.5 M, `ets_copy` 250 000/2 M, `send_recv` 90 000/0.5 M, `send_recv_plain` 300 000/1 M; `gc_bound` `churn` 10 000/200 000; `gc_class` `send_recv` 25 000/200 000, `ets_copy` 40 000/400 000, `compile` 2/8; `file_io` `write_read` 20 000/200 000, `handle_rw` 1 000/200 000, `ram_file` 20 000/200 000 (moot on rust — the case is absent there); `binary_append` 2 000/50 000 calls.
  `binary_append`'s rust count is **capped by the runtime, not by the loop-count rule**: `beam_rt`'s `RUN_DEADLINE = 60 s` (`crates/beam_rt/src/runtime/scheduler.rs`) is **non-overridable** (no env var, no flag), so its per-call cost can only reach `n_rust` = 2 000 before the `run` action is killed — a dearer call turns that row into `failed(no-binary-append-run)` rather than a longer measurement (its pinning test bounds the work from above as well as below).
  **Maintenance rule: when a case's measured per-unit cost changes materially, re-derive that case's `n_rust` so the loop work stays ≥0.5 s and re-pin it** in the file's `the_rust_loop_count(s)_clear_the_boot_jitter_band` test (which also fails when a case is added without a measured cost). Each case table's doc comment states the rule and the measured per-unit cost.
- **Budget rule.** A budget row that names a workload absent from the report (not registered, not selected, or a runtime that was not run) is `NotMeasured`, **never** a pass and **never** a failure.
  A limit is only changed when a measured figure changes — and when one is breached the breach is **reported**, not silently re-tuned to fit (the rationales in `src/budget.rs` quote the figures each limit was tuned against, so a re-tune is visible in the diff).
- External-repo roots are **documented defaults**, overridable via `--repo <id>=<path>` (repeatable) and env `BENCH_<ID>_REPO` (e.g. `BENCH_COWBOY_REPO`); no other absolute path is baked in, and the store scan never bakes in a hash or version.
- **Never write into the external repos.** Any build/run is staged into a `ScratchDir` copy first (and a tool's writable dirs — `HOME`, `MIX_HOME`, `TMPDIR`, … — are redirected into that copy). Never fabricate a metric.
- **Peak memory:** Linux `/proc/<pid>/status` `VmHWM` polling (pure `std::fs`, no `unsafe`) while the child runs; if unreadable ⇒ `peak_memory_bytes: null` + a `notes` entry.
  Erlang-reported `erlang:memory(total)` may be recorded as a *separate* named metric, never conflated with OS RSS — and **its rust value is an accounting figure, not a footprint**: the runtime charges itself only for live capacity, so its `vm_memory_total_bytes` can fall while its OS peak RSS rises (never read the resulting ratio as a memory win; the like-for-like footprint reads are `slice`'s peak RSS and the binary A/B's per-workload RSS rows).
- **Measure one process at a time on this shared host** and record `/proc/loadavg` around every run; read ratios and the deterministic counters, not absolute wall times.
- Files target ~1000 lines; split by responsibility before growing past it.

## Known Issues / Notes for Agents
- **Spawn goes through a bounded `ETXTBSY` retry** (`proc::spawn_with_retry`, mirroring the differential harness): both `proc::run` and `proc::spawn` retry ONLY the transient `ExecutableFileBusy` / raw-errno-26 fork/exec race (≤5 attempts, ~10 ms backoff) and return every other error kind unchanged.
  It is a bounded retry of a proven scheduling artifact, NOT error swallowing — do not remove it. `Runtime::self_check_result` surfaces the errors that outlive it, and `Runtime::self_check() -> bool` stays a thin `unwrap_or(false)` wrapper for `info()`/`oracle_smoke`.
- Build/test via the wrapper (read-only `$HOME`): from the repo root `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench`; a real run: `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- run --quick --out-file erts_rust/tools/bench/out/target-bench.json`. Never hardcode the worktree path.
- Toolchain in this sandbox: `erl`/`erlc`/`escript`/`make`/`mix`/`elixir`/`curl`/`git`/`nix`/`jq`/`clang` **present**, and **`dotnet` IS on `PATH`** (SDK **9.0.318**; the store also holds a `dotnet-sdk-9.0.318` and two 8.0.425 SDKs).
  `rebar3`, `wrk`, `h2load`, `bombardier`, `ab` and `/usr/bin/time` are **absent from `PATH`** — `rebar3` resolves from the Nix store (`rebar3-3.27.3`) via the shared store scan.
  `automake`/`libtool`/`cmake` are absent (the `emqx` blocker (2)); an OTP-28 `erl` **is** reachable in the store (`erlang-28.5.0.5`) but insufficient (below).
  Network is available (erlang.mk can git-fetch cowlib/ranch). Oracle is OTP 29.1.1 / erts-17.1.
  After `scripts/cargo.sh build --release --workspace`, `../../target/release/beam_rt` is built ⇒ the `rust` runtime resolves and passes the self-check.
- **`$BENCH_TOOL_PATH`** (colon-separated `bin` dirs, searched before the store scan) is the documented way to force a specific tool build; **`$BENCH_TRY_NIX`** (opt-in, default off) is the last-resort `nix shell nixpkgs#<attr> --command <tool>` fallback. Neither is required for the default path.
- **`emqx` stays `skipped(emqx-requires-otp-28)`, and the skip is honest and evidence-backed.** The checkout pins OTP **28.5.0.7-2** (`.tool-versions:1`, `env.sh:3`) while the host `erl` is OTP 29.1.1 / erts-17.1.
  Blocker (1) is OTP-release-specific: `:ezstd`'s `env.mk` is generated via `erl -noshell -s init stop -eval …`, which OTP ≥ 29 halts before evaluating, while an OTP-28 `erl` **does** evaluate it.
  Blocker (2) is **release-independent**: `automake`/`libtool`/`cmake` are not resolvable (the gate **probes** them per run via `absent_build_tools()`) and `krb5.h` is off the default include path, so `:jq`/`:rocksdb`/`:quicer`/`:sasl_auth` cannot build — which is why per-workload OTP-28 support is deliberately **not** implemented (it would turn an honest skip into `failed(emqx-build-failed)` for a host-specific toolchain gap), even though an OTP-28 `erl` is reachable in the store (`erlang-28.5.0.5`).
  The full enablement ladder is `results/m12-emqx-enablement-attempt.md`; the re-proof is `results/m19-target-notes.md`.
- **`khepri` runs, and its row records which Erlang the suite actually ran under.** The store `rebar3` is an **escript whose shebang names its own Erlang** (`shebang_interpreter`), so the suite may run under a slightly different OTP than the `runtimes[]` row describes; the row states the resolved `rebar3` and that interpreter, so no metric is fabricated or read without its provenance.
- **`plb` requires a ≥ 9.0 `dotnet`; `command_for_min` finds it by version.** PLB's elixir envs target `net9` (`bench/tool/BenchTool.csproj`, `bench/include/dotnet/app.csproj`), so a too-old `PATH` `dotnet` cannot shadow a newer store SDK (smoke-verified with a real 8.0.425 forced to the front of `PATH`).
  No reachable `dotnet` at all ⇒ `skipped(dotnet-absent)`; one reachable but none ≥ 9 ⇒ a genuine `failed(plb-build-failed)` carrying the `requires >= 9` notes — never a fabricated `ok`.
- **Cowboy** builds a relx release in the scratch copy (network fetch of cowlib+ranch); the release start script `foreground` **`exec`s** `erlexec`, so killing the spawned child stops the VM and its pid is the Erlang VM (so `VmHWM` polling works).
- **The benchmark host is shared and usually loaded.** Absolute latencies/throughputs move several-fold with load, because the oracle `erl` (a 14-scheduler SMP VM) loses more to CPU contention than `beam_rt`.
  Prefer the **quiet** run and ratios over absolute values, always record `/proc/loadavg` with the numbers, and never quote a single contended run as a constant (the load record is in `RESULTS.md` and `results/m19-*-load.log`).
- **Elixir**: the in-repo checkout (`/home/bill/Source/elixir`) is unbuilt; the workload measures the installed system `elixirc`. Compiling the checkout's stdlib in one `elixirc` batch fails on version skew, hence the per-file fallback.
- The external-repo CONTEXT.md files (added read-only, no source changes) record each repo's build/bench entry points; consult them via `--repo` overrides.
- `out/` is gitignored by `../../.gitignore` (`/tools/bench/out/`); the standard scratch artifact is `out/target-bench.json` (via `--out-file`). Durable, reviewable results go to the **tracked** `results/` directory, which is for committed milestone numbers only.
- **`foreign`'s and `binary_append`'s `case_<name>_ns_per_call` are N-dependent on the Rust runtime**, and the loop workloads' rust peak RSS is **boot-dominated** — so those per-call/RSS rows are not a clean cross-runtime ratio.
  Both facts, with the owner `file:line`s and root causes (which live **outside this node**), are in the dedicated *Known Issue* sections below; every row emits its `case_<name>_calls` companion and an explicit note.
- **`file_io`'s `ram_file` is absent on the rust side** (`beam_rt` cannot serve a `ram` file), so that case carries no rust metric and its oracle figure is a **sys-only** number — read a `file_io` cross-runtime comparison on `write_read`/`handle_rw` only.
- **The per-unit gaps against the oracle are the standing handoff.** Every per-unit case is dearer than the system oracle; `RESULTS.md` carries the current ratio and a one-line diagnosis per case, and the binary A/B attributes a change to code rather than to host load. Keeping the measurement honest and reproducible is this node's job; the fixes are not.

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `stats` (percentile/mean edge cases), `metrics`/`report` (JSON round-trip + schema stability + exactly-one-of value/summary + the defaulted `budgets` section), `cli` (every subcommand, defaults, `--help`, `--out-file`, `--fail-on-budget` on `run`/`baseline` and its rejection on `list`/`compare`, error cases), `compare` (deltas), `scratch` (RAII/copy/skip), `http_load` (loopback keep-alive server + pure parsers), `marginal` (the pinning test `interleaving_cancels_a_drifting_boot_cost` — a fake clock proves the paired median recovers the true per-unit cost exactly where the old sequential `mean(work) − mean(startup)` is off by `runs × drift_step` — plus the median-vs-outlier, noise-band, degenerate-input, pair-retention and `Pass` round-trip cases), `budget` (table coherence, the stat/limit/verdict accessors and tokens, the `NotMeasured` reasons, the JSON round-trip, the render lines/block), `run` (a breach is not fatal without the flag; a `Met`/`NotMeasured` budget never fails a run), `workloads::tool` (tool/argv + the version-aware store ranking + `command_for_min`'s selection: greatest satisfying version, tie-break by source order, fallback note, `parse_version_output`, provenance/rejection notes), `workloads::{foreign, list_walk, copy_bound, gc_bound, gc_class, file_io}` (case tables, per-side argv, the paired per-op/per-call/per-element math, deterministic stdout, the loop-count rule) and `workloads::{plb, khepri, emqx}` (the honesty/enable-and-skip decisions: `plb`'s excluded `timeMS = 0` results and no-result-set ⇒ `failed`; `khepri`'s shebang reading; `emqx`'s gate note naming the absent build tools).
  Each loop workload pins the three sampling guarantees — `rounds_are_paired_work_then_startup` (round *i* is Work-then-Startup), `a_failing_startup_pass_keeps_the_completed_pairs` (the completed rounds are still reported) and `the_metric_is_withheld_inside_the_noise_band` (no figure from a sub-noise marginal) — plus its recalibrated counts in `the_rust_loop_count(s)_clear_the_boot_jitter_band`.
  `file_io` additionally pins that only `ram_file` is optional and that an optional case which refuses is `absent` with a reason while a *required* case's failure is never downgraded; `gc_class` pins the compile-attribution classification and the `out1..out<n>` creation.
  The tool-driven targets' **enable/skip decisions** are pinned so a future environment change cannot silently turn a real measurement into a skip — `khepri` probes `Ready` and its resolved `rebar3` exists on disk; `plb` probes `Ready` and its version-resolved `dotnet` satisfies `MIN_DOTNET_SDK`; `emqx` gates a pinned checkout on a newer host. Each early-returns when its prerequisite is absent, so CI stays green.
- Library integration tests: `tests/integration.rs` (a synthetic in-memory `Report` serialized, reloaded and compared — hermetic, no oracle needed — plus the `Runtimes::Both` regression guard for the six loop workloads) and `tests/budget.rs` (the budget mechanism against archived reports; see the routing table).
- Oracle-dependent smoke tests (`baseline`; the loop workloads per runtime) run only when `erl`/`erlc` resolve, and are a no-op skip otherwise, so `cargo test -p bench` stays green in any environment.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` GREEN and `... test --workspace` GREEN, plus a demonstrated `run --quick` producing real numbers for every workload that is not honestly skipped. The milestone gate is the `--full` run archived in `results/` and summarised in `RESULTS.md`, which must be refreshed whenever the numbers change.

## Runbook
Nothing here is required for a code review. **`RESULTS.md` §7 is the authoritative, current runbook** (the exact `bench run …` commands behind the `results/m19-*` artifacts); reproduce from there rather than from a copy.
The shape is: build the driver *and* the runtime once (`erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench` from the repo root), then drive the subcommands.
The current session's shape:
- **the per-unit record is six matched rounds** — one single-side `--quick` pass per side per round, alternating the side order, each pass pinned with `taskset -c 3`, one process at a time — *not* a runtime-major `--runtime all` pass (all-`sys`-then-all-`rust` samples the two sides minutes apart, and m16 measured that ordering biasing a per-case ratio by up to 18 % on a drifting host);
- **a `--full` `baseline` pair** for the startup distributions;
- **three reps per external target** (the `SysOnly` rows), which stay **unpinned** so they remain comparable with the earlier external sessions;
- **one `--runtime all` budget-evaluation pass** over `baseline,list_walk,copy_bound,gc_bound,file_io,gc_class,cowboy` (the only pass that can judge the ratio rows, since a ratio needs both sides in one report).
Two facts to keep in mind when reproducing:
- A `--runtime all` report shows a genuine sys-vs-rust row only for the nine `Runtimes::Both` workloads (`baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound`, `gc_class`, `file_io`, `binary_append`).
- **Always measure one process at a time** and record `/proc/loadavg` before and after every pass; the host is shared, so read ratios + the deterministic `case_*_ops/calls/elements` counters rather than absolute latencies. Pin binary-vs-binary A/B with `taskset` and alternate its side order round by round.
- Archive the `out/` copies into `results/` with only the mechanical path normalisations (`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **Never edit a metric value.**

## Status
- The driver is a complete library + binary: **15 workloads**, all implemented and tested. `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p bench --all-targets` is **warning-free**, and `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` is green at **343 passed / 0 failed** (320 lib unit + 14 `tests/budget.rs` + 4 `tests/integration.rs` + 1 `tests/oracle_smoke.rs` + 4 doc-tests).
- **Current measurement session: `m19` (2026-10-23/24, 1-min load 0.54–4.53 — a quiet window; the matched passes and the `--full` pair pinned with `taskset -c 3`, externals unpinned).** Twelve matched single-side passes × the nine `Runtimes::Both` workloads, all `ok`; the `--full` `baseline` pair; the three-rep external stage over five targets plus the `emqx` skip, all `ok`/`skipped` with **no `failed` row and no withheld marginal**; and the budget-evaluation pass, whose only `NotMeasured` verdict is `cowboy` in the run that did not select it. Loads per pass in `RESULTS.md` and `results/m19-*-load.log`.
- **The cross-runtime figure is the median of the six matched rounds' per-round ratios**; the per-round ratio column in `RESULTS.md` is the stability evidence, and the sys side is far more load-insensitive than the rust side, so an absolute rust wall time can move several-fold across a session while the ratio moves tens of percent.
- **A run now also carries its own judgement**: `report.budgets` holds one verdict per budget row, so a saved report states whether the run was acceptable without anyone re-running or hand-comparing it.
- The figures themselves live only in `RESULTS.md` and in `results/m19-*`; this file restates none of them as a number. Older figures are history: `m16-bench-xrun-interp.json` remains the interpreter-tier control, which no later session re-ran.

## Reading results — where the numbers live, and what they mean
**`RESULTS.md` is the authoritative source for every measured number** (per-category tables with both sides named, the environment/load record, the reproduction commands, the calibration rule, the still-open items); the raw reports behind it are committed under `results/`. This file restates no number — read them there and regenerate via the Runbook rather than quoting a stale copy.
Structural facts that are not numbers (and so belong here):
- `baseline`'s compute loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1`, so a near-parity result reads as the **native floor**, not execution throughput.
  The per-unit execution signals are `list_walk` (ns/element), `foreign` (ns/call of real foreign code), `gc_class` (ns/round of a collector-running class), `file_io` (ns/op of a file-system round trip) and `binary_append` (ns/call of in-place binary growth).
- `slice` runs one call per process (`N`=1), while `foreign`/`list_walk`/`copy_bound`/`gc_bound`/`gc_class`/`file_io`/`binary_append` loop many ops per process at a per-runtime `N`, so peak RSS is like-for-like **only** for `slice`.
- **Rust-vs-Rust rows** (the interpreter-pin tier control and the binary-vs-binary A/B) are labelled separately and are **never** a system-ERTS comparison.
- A **`sys`-only** figure (every external target, and `file_io:ram_file`) is a whole-program number for that target's own toolchain, never half of a ratio.

**The per-unit gaps are real runtime costs, not harness artifacts**: the startup-cancelled metric reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`, so CLI re-entry, per-call module reloads and per-call GC are ruled out, and what remains is a `beam_rt` per-call host-context cost owned by `../../crates/beam_rt/PERF.md` (not `bench` output).
Startup latency is therefore the trustworthy cross-runtime comparison for whole-process cost (and, on `slice`, peak RSS).

**The `beam_rt` execution tier defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit` on a process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the only kill switch), compiling **per function, lazily**, so a fresh process pays only for the functions it enters; the eager whole-module artifact serves introspection and the `beam_engine` `jit_bench` harness only.
**The tier effect is measured here** as matched-form `--quick` rust-only passes: the interpreter control `results/m16-bench-xrun-interp.json` against that session's default-tier partner, with `BEAM_RT_TIER` exported (`bench` spawns the runtime with an inherited environment, so the switch reaches every child). They have **no `sys` rows**, no later session re-ran the control, and `RESULTS.md` tabulates them.

> **`ERTS_STDLIB_DIR` note:** the Rust runtime resolves its system `ebin` path with a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`; the system `erl` is *run* only as a last resort for an installation the layout cannot see).
> `bench` does **not** set it — the harness is a pure driver/observer and never mutates the runtime's environment beyond argv (and the per-unit metric cancels any remaining startup cost by construction).

## Known Issue — the `copy_bound` payload is a module literal, and the two copiers treat literals differently

`payload/0` returns a **compile-time constant**, so `T` is a literal-tagged word into the module's literal area (load-time tagged; `../../crates/beam_rt/src/context/CONTEXT.md`), never a term built at runtime.
The four cases therefore copy a *wrapper* around a literal, and the two copiers disagree on literals: `beam_message`'s `is_shared` shares a literal-area term (0 destination words, `../../crates/beam_message/src/copy.rs` + `CONTEXT.md`), while the oracle's message path passes `litopt = NULL` (`erts/emulator/beam/global.h` `copy_struct` macro; `copy.c` skips literals only when `litopt` is set), i.e. real ERTS **copies** the literal's interior into the receiver heap.
Consequence for reading the rows: in `send_recv_plain` the oracle copies the payload's 70 words per send while the Rust side shares them (so that case is *not* an identical-work comparison — the ping-pong dominates both sides); in `send_recv` the worker's `binary_to_term/1` forces a genuine heap copy on **both** sides, so the codec attribution (`send_recv − send_recv_plain`) is the more load-bearing figure.
`gc_class` sidesteps the whole issue: its payload is rebuilt every round and deliberately exceeds the nursery, so both sides copy *and* collect.

## Known Issue — the `foreign` per-call metric is `N`-dependent, and the loop workloads' rust peak RSS is boot-dominated

### `foreign` `case_<name>_ns_per_call` is N-dependent on the Rust runtime
On `beam_rt` the per-call cost still drifts with `N` — a **common-mode per-op heap retention** — so `foreign`'s `case_<name>_ns_per_call` remains only a clean marginal at the declared `N` and is **only comparable across runs that used the same `N`**; the `sys` oracle side is ~flat over the same range.
The rust counts are the ones in Constraints, so every earlier session's per-call figure is a different baseline. `RESULTS.md` records the verdict; history: the m17/m16 binary A/B sets, the m13 A/B set, `results/m12-foreign-regression.md`, `results/b54-foreign-growth-curve.md`.
This workload's job here is honesty, not any fix: every row emits the deterministic `case_<name>_calls = N` companion, and the Rust row carries an explicit N-dependence note, so a per-call figure is never quoted without the `N` behind it.

### the loop workloads' rust peak RSS is boot-dominated
The loop workloads' rust peak RSS is **boot-dominated** — the resident preloaded-module set of the booted `start.boot` dwarfs the loop's own footprint, so those rows read far above the oracle's and are **not** a like-for-like memory comparison (`RESULTS.md` carries the per-workload RSS rows).
Beneath it the same **general per-op heap retention** remains the underlying mechanism: the BIF-result copy-out (`crates/beam_rt/src/context/calls.rs`) and the engine's unrooted `alloc_words` seam (`crates/beam_rt/src/context/gc.rs`) both call `Process::grow_nursery` (`crates/beam_process/src/process.rs`) → `Nursery::grow` (`crates/beam_gc/src/nursery.rs`), which pushes the replaced whole `HeapSpace` onto `Nursery.retired`; `retired` is cleared only by a real collection (`Nursery::collapse`), which these loops do not necessarily trigger.
The per-op `.erl` `ByteSize`/`tuple_size` checks still make a silently-dropped allocation fail loudly — this is a retention, not a correctness, issue.
So the rust `foreign`/`copy_bound`/`file_io`/`gc_class` per-case `case_<name>_peak_rss_bytes` is **not** a like-for-like memory comparison (loop-length- **and** boot-dependent); `slice` (one call per process, `N`=1) remains the comparable per-process memory metric.

**Reproducing the counter-level evidence (the driver prints none of it).** A loop row records only `case_<name>_{wall_ms,startup_ms,peak_rss_bytes,ns_per_*,ops}` plus notes — never `collections`/`heap_size`.
Those per-process counters need a probe module of your own (`erlang:process_info(self(), heap_size | total_heap_size | garbage_collection)` + `erlang:statistics(garbage_collection)`) calling the staged case function through the **`run` action** (`beam_rt -pa <dir> run <probe.beam> <probe>:<f>/1 <N>`); the `-eval` route leaves the counters at their fresh values even while peak RSS grows.
(`heap_size` is `Nursery::capacity_words` — the sum over retired + current spaces — so it is the retained-capacity signal.) The measured values and root causes are the `beam_rt` node's.

## Honesty audit — measurement-integrity notes (current state)
No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`, `src/budget.rs`).
No `todo!`/`unimplemented!`, no `#[ignore]`; the only `unreachable!`s (three in `src/compare.rs`) are invariant assertions carrying an explanatory message (an index-union / impossibility guard), not silent error handling.
A budget verdict is honest by construction: `NotMeasured` is never a pass, `evaluate` cannot invent a measurement, and a ratio row with no usable `sys` denominator says so instead of dividing by a guess.

- **Staging vs compiling is distinguished** (`src/workloads/foreign.rs`: `StageError` + `stage_outcome`): a checkout whose foreign `.erl` sources are genuinely absent ⇒ `skipped(sources-missing)`, while failing to compile the sources we *did* stage (the embedded driver or a present foreign source) ⇒ `failed(module-compile-failed)` — matching the workloads whose sources are ours, and `gc_class` distinguishes a missing sibling fixture (`skipped(fixtures-missing)`) from a non-compiling one (`failed`).
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` come from the *produced* counts** (`rates_per_second` over `CorpusRun::produced`/`produced_loc`, the latter matched back to source by file stem), so a partial `erlc` run cannot overstate throughput next to `files_compiled`; the attempted totals survive only in a note, and `elixir_compiler` uses the produced count too.
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** (the threads only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic); the invariant is documented there.
- `baseline.rs` states correctly that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a fallback.

The loop workloads' methodology is implemented exactly as `RESULTS.md` states (the paired marginal with a per-runtime `N` calibrated per the loop-count rule); the registered-BIF dispatch cost is **not** produced here — it is a `beam_rt` measurement (`../../crates/beam_rt/PERF.md`, `TRACE_AND_PERF.md`).

## See Also
- `RESULTS.md` → the authoritative results summary (per-category tables, both sides named, the matched-round methodology, the external targets, the latency-budget verdicts, exact reproduction commands, verified skip reasons, the load record, the calibration rule, the paired-estimator record, the open items).
- `results/` → the raw harness behind `RESULTS.md` (committed, so the numbers stay reviewable); see the routing-table entry above for the `m19-*` set and the history groups.
- `results/m19-target-notes.md` → the activation evidence: `plb` and `khepri` running with their SDK/Erlang provenance, and the re-proved `emqx` skip with the exact commands and outputs behind both blockers.
- `results/m12-emqx-ezstd-env-mk.patch` → the literal, reproducible `:ezstd` `env.mk` idiom patch (a *fetched* dep, applied after `mix deps.get`; **insufficient alone** — the build then needs the missing host `automake`/`libtool`/`cmake`/`krb5`), which is why the driver applies no patch and keeps its honest skip.
- `results/m12-emqx-enablement-attempt.md` → the verified, bounded `emqx` enablement attempt (exact commands, each blocker's exact failing output).
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy, the differential gate, staging.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the concrete target workloads + build/bench entry points.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I → the mandated success metrics (startup, p99, throughput, memory) that the budget rows make explicit.
