# Benchmark Driver (`erts_rust/tools/bench`) — `bench` crate

## Intent
`bench` is the in-repo **benchmark driver** for the Rust ERTS rewrite. It builds and runs the external
pure-Erlang/Elixir target workloads named in `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` and **always
records the system-ERTS baseline**, capturing the success metrics mandated by
`../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I: **startup latency, p50/p99 tail latency, throughput,
peak memory**.
It is a **library + binary** crate: the binary (`bench`) is what a human or CI drives; the library exists
so `cargo test -p bench` can assert the harness itself (statistics, JSON-schema stability, probe logic,
CLI parsing, report comparison).
`bench` is a **pure driver/observer**: it never modifies the Rust runtime and never writes to the external
repos. Full suites are milestone-only (Part I §C); the default `--quick` path stays bounded.

## Shared conventions (binding — from `../CONTEXT.md`, the `tools/` node)
- **Runtime resolution — documented, overridable, no baked absolute paths:**
  - **Oracle (`sys`)** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud env error (exit 2).
  - **Rust (`rust`)** = `$BEAM_RT` if set, else the workspace-relative `../../target/{release,debug}/beam_rt`
    (resolved from `CARGO_MANIFEST_DIR`).
  - Both are invoked with the **same argv as `erl`** — the runtime binary is treated as a drop-in `erl`
    (contract A.6). Only the *program path* is set; internals are never assumed.
- **Honest reporting:** a case/workload that cannot run is `skipped(<reason>)` — **never** `pass`, and
  metrics are never fabricated. Unknown/unavailable ⇒ explicit reason string.
- **Machine-readable results:** JSON with a stable schema (below), written under the gitignored `out/`
  dir (already ignored by `../../.gitignore`: `/tools/bench/out/`) unless `--out` overrides.
- **No heavy new deps:** `serde`/`serde_json` (already workspace deps) for JSON; **hand-rolled `std`-only
  arg parsing** (no `clap`). `beam_common` (already a dep) may be used only for its logging facade.
- Files target ~1000 lines; split by responsibility. Apache-2.0.
- `unsafe` is **forbidden** (`#![forbid(unsafe_code)]`).
- External repos are **read-only** (`/home/bill/Source/{elixir,cowboy,khepri,emqx,Programming-Language-Benchmarks}`);
  anything that must build/run is staged into `$TMPDIR` first (see the read-only target adapter below).

### Read-only target adapter (`src/scratch.rs`)
A reusable strategy so workloads never mutate the (read-only) source repos: `ScratchDir` is an RAII temp
dir under `std::env::temp_dir()` (unique name, auto-removed on `Drop`); `copy_tree(src, dst, skip_names)`
recursively copies a repo subtree (skipping named entries like `.git`/`_rel`/`deps` at every depth,
preserving modes); `copy_files(files, dst)` copies an explicit file list. Workloads stage the repo into a
`ScratchDir`, build/run **there**, and let the `Drop` clean up. `--repo`/`$BENCH_<ID>_REPO` still select
the *source* tree. Verified: source repos stay untouched (`git status` clean) after a run.

### `beam_rt` is a REAL runtime (self-checked)
`beam_rt` is a working Rust runtime (real `-eval` evaluator, a global-atom `code:*`/`init:*` floor, native `os:cmd/1`); it is resolved from `$BEAM_RT` or the workspace-relative `../../target/{release,debug}/beam_rt`.
A runtime is *supported* only if it can execute the canonical probe `-noshell -eval 'io:format("~p~n",[1+1]), halt().'` and print `2`.
A resolved, probe-passing runtime reports `supported: true` (the reported version comes from `erlang:system_info(otp_release)`; `beam_rt` prints `30`), so a workload scheduled under `rust` **runs**.
An unresolved or probe-failing runtime is honestly `skipped(runtime-unsupported)`, **never** a fake number.

## API Surface

### CLI (hand-rolled; usable `--help` on every subcommand)
- `bench list [--json]` — list workload ids, category, contract coverage, and current probe status.
- `bench run [--quick|--full] [--workloads <ids,csv>] [--runtime <sys|rust|all>] [--out <dir>] [--out-file <path>] [--repo <id>=<path>] [--json] [--verbose]`
- `bench baseline [--quick|--full] [--json] [--out <dir>] [--out-file <path>]` — measure **only** the system-ERTS baseline.
- `bench compare <report.json> [--json]` — **cross-runtime**: diff the `sys` rows against the `rust`
  rows of one report (as produced by `bench run --runtime all`).
- `bench compare <a.json> <b.json> [--json]` — **two-file**: diff two result sets (`a` = baseline).
- `bench help [<subcommand>]` — top-level or per-subcommand help.

Defaults: `run` ⇒ `--quick`, `--runtime sys`, all workloads. Report file = `<out>/bench-<profile>-<unix_ms>.json`
(`<out>` = the crate's `out/` dir), or the **exact** path given by `--out-file <path>` when set.
Exit codes: `0` = report produced (per-workload `skipped(reason)` and `failed` are recorded in JSON,
not fatal); `2` = usage/env error (bad flag/arg, or the oracle cannot be resolved for `run`/`baseline`).
`compare`: `0` = compared, `2` = usage/IO/schema error. A workload that *errors* at runtime is recorded
`failed` in JSON (does not change the process exit code).

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
`runtimes()` gates scheduling; a workload scheduled under a runtime it does not support ⇒
`Skipped("runtime-unsupported")`. A `Probe::Skipped(reason)` ⇒ `Skipped(reason)` without running.

### Built-in workloads
| id | category | target repo / tool | today | skip reason(s) |
|---|---|---|---|---|
| `baseline` | startup | oracle `erl` | **runs** (sys+rust) | `runtime-unsupported` (both runtimes report `erlang:memory(total)`; a capability gap still degrades, see below) |
| `erlc_compiler` | compiler | `erlc` over OTP stdlib **+** Elixir `lib/elixir/src` `.erl` sources | **runs** (quick+full) | `erlc-absent`, `corpus-missing` |
| `elixir_compiler` | compiler | system `elixirc` over a staged `.ex` corpus from the Elixir checkout | **runs** (quick+full) | `elixirc-absent`, `corpus-missing`, `repo-missing` |
| `plb` | compute | `/home/bill/Source/Programming-Language-Benchmarks` (`dotnet` driver) | skipped(dotnet-absent) / failed(plb-tool-failed) | `dotnet-absent`, `repo-missing`, `plb-tool-failed`, `plb-no-results` |
| `cowboy` | io | `/home/bill/Source/cowboy` hello_world built in a scratch copy, driven by the in-process loader | **runs** | `repo-missing`, `make-absent`, `deps-unavailable` |
| `khepri` | gc | `/home/bill/Source/khepri` (`rebar3 eunit`, in a scratch copy) | **runs** (with `rebar3` on `PATH`) | `rebar3-absent`, `repo-missing` |
| `emqx` | messaging | `/home/bill/Source/emqx` (`rebar3 compile`, in a scratch copy) | skipped(not-a-rebar3-project) | `rebar3-absent`, `not-a-rebar3-project`, `repo-missing` |
| `slice` | compute | committed `slice.beam` fixture + 6 `erlc`-compiled stdlib wrappers, run under **each scheduled runtime** | **runs** (sys+rust) | `erlc-absent`, `fixture-missing`, `runtime-unsupported` |
| `foreign` | compute | **real Cowboy + EMQX modules** (`cowboy_bstr.erl`, `emqx_base62.erl`) compiled with the system `erlc`, run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `sources-missing`, `runtime-unsupported` |

- `baseline` (always-on): N runs of `<erl> -noshell -eval 'halt().'` ⇒ startup-latency `Summary` (p50/p99);
  a fixed in-VM compute loop `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'`
  ⇒ `compute_loop_ms` + `compute_ops_per_sec`; and a **separate** VM-memory probe
  `-eval 'M = erlang:memory(total), io:format("~p~n",[M]), halt().'` ⇒ the scalar `vm_memory_total_bytes`
  (never conflated with the OS `peak_memory_bytes`, which is the compute runs' RSS). `--quick` uses 10
  startup runs / 5 compute runs; `--full` 50 / 5. **Both runtimes produce all three metrics:**
  `beam_rt` implements `erlang:memory/0,1` through the `MemoryHook` seam, so the rust memory probe
  records a real figure (see `RESULTS.md` §3.2). **Capability gaps degrade, they never abort:** if a runtime does not implement
  a probed BIF, the probe records **no** metric plus an explicit `… : not-run (unsupported: …)` note,
  while the compute loop, the startup summary and every other metric are still produced. A genuine
  (non-capability-gap) probe failure is surfaced verbatim, never fabricated. Note: the loop is
  dominated by the natively-served `lists:seq/2`+`lists:sum/1`, so a near-parity rust figure reflects the
  native floor, **not** interpreter throughput (that is `foreign`).
- `erlc_compiler`: compiles a bounded deterministic slice (first 30 `.erl` of each corpus) of the in-repo
  OTP `lib/stdlib/src` **and** the Elixir checkout's `lib/elixir/src` (both read-only) with `erlc` into a
  `ScratchDir` ⇒ files/sec, LOC/sec, wall time, peak memory. Missing/failing Elixir corpus falls back to
  OTP-only with a note.
- `elixir_compiler`: measures the **installed system `elixirc`** (the in-repo checkout is unbuilt/read-only)
  over a fixed `.ex` corpus staged into a `ScratchDir` ⇒ wall time, files/sec, LOC/sec, compiler-startup,
  peak memory. Tries one batch `elixirc`; on failure/short-output falls back to per-file compilation
  (robust to the checkout↔installed version skew) and notes which mode ran.
- `cowboy`: `copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk
  git-fetches cowlib/ranch; a scratch-copy relx build), starts `_rel/hello_world_example/bin/hello_world_example foreground` on port
  8080, drives it with `http_load::drive` (in-process keep-alive HTTP/1.1), records build/latency/throughput
  and the server's peak RSS, then stops it. Build/dep failure ⇒ `skipped(deps-unavailable)`; compile error ⇒ `failed`.
- `plb`/`khepri`/`emqx`: staged into a `ScratchDir` and built/run there (`dotnet` / `rebar3`); tooling
  absent ⇒ honest `skipped(<tool>-absent)`. The external benchmark drivers (`khepri-benchmark`,
  `emqtt-bench`) are outreach/future work. **`emqx` is not a `rebar3` project** (it builds via
  `make`/`mix`; no root `rebar.config`), so `rebar3 compile` there is a silent no-op that exits `0`
  without compiling anything — the workload detects that (root `rebar.config` missing) and reports the
  honest `skipped(not-a-rebar3-project)` instead of a meaningless wall time.
- `plb` honesty rule: a `dotnet` run that yields **no** result JSON is recorded `failed`
  (`plb-tool-failed` when the tool exited non-zero, `plb-no-results` when it exited `0` having benched
  nothing) — **never** `ok`, because the only other number on such a row is the tool's own wall time,
  which is not a benchmark result.
- `slice`: runs a fixed table of **module-run cases** under whatever runtime it is scheduled on
  (so under `--runtime all` it is a real sys-vs-rust speed + memory comparison of the *same* programs).
  3 cases use the committed `slice.beam` fixture (`add/2`, `fib/1`, `pick/1`); 6 use tiny embedded
  stdlib/kernel wrapper modules compiled once with the system `erlc` (calling a *real* system module:
  `calendar`/`queue`/`gb_trees`/`base64`/`ordsets`/`string`). `--quick` ⇒ 5 timed runs/case,
  `--full` ⇒ 25. Metrics are per-case summaries `case_<name>_wall_ms` / `case_<name>_peak_rss_bytes`
  (peak RSS = external `VmHWM`); an `observed stdout:` note records the agreement, and a Rust exit `3`
  (`unsupported: …`) yields a note and **no** timing (never a fabricated number). Because a `beam_rt`
  process resolves the system code path by spawning `erl` once when a case first needs a system module,
  the wrapper cases each pay one `erl` startup; the pure-fixture cases do not.
- `foreign`: **the workload that measures the Rust runtime on real foreign target-project code.**
  Every repo-backed workload is `SysOnly` (the Rust runtime cannot boot an OTP release, run `rebar3`,
  or act as `erlc`/`elixirc`), so `foreign` instead picks **pure, self-contained modules from the
  Cowboy and EMQX checkouts** (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), copies
  the `.erl` sources into a `ScratchDir`, compiles them **once** with the system `erlc` together with
  an embedded driver (`bench_foreign_driver`), and runs the identical `.beam` under **whatever runtime
  it is scheduled on** — under `--runtime all` that is a genuine side-by-side of the *same* compiled
  code. Sources are read-only; all `.beam` output lives in the self-cleaning scratch dir.
  Measurement is **startup-corrected per call**: each case declares a per-runtime loop count `N`
  (calibrated so a run costs a few hundred ms on Rust and gives an oracle marginal well above the
  startup noise) and the program is run twice per sample — at `N` and at `0` iterations — giving
  `case_<name>_wall_ms`, `case_<name>_startup_ms`, `case_<name>_peak_rss_bytes`, and the throughput
  metric `case_<name>_ns_per_call = (mean(wall@N) − mean(wall@0)) / N × 1e6`, emitted only when positive
  (else an honest note). Every runtime's stdout must equal `N × per_call` (the driver's accumulator is
  linear in `N`), recorded in an `observed stdout:` note; a Rust exit `3` records the capability gap and
  emits **no** timing. All four cases run under both runtimes today. The calibrated loop counts are
  `bstr_to_lower` 20 000 / 1 500 000, `bstr_capitalize_token` 15 000 / 1 500 000, `emqx_base62_encode`
  25 000 / 3 000 000, `emqx_base62_decode` 15 000 / 3 000 000 (rust / sys). `case_<name>_peak_rss_bytes`
  scales with the rust loop count because `beam_rt` does not yet drive a garbage collector (see Known
  Issues), so `foreign` peak RSS is **not** a like-for-like memory comparison — `slice` is.

### Result JSON schema (stable — `SCHEMA_VERSION = 1`)
Written by every `run`/`baseline`; consumed by `compare`. Durations/latencies in **milliseconds** (f64).
```jsonc
{
  "schema_version": 1,
  "tool": "bench",
  "tool_version": "0.1.0",
  "generated_at_unix_ms": 1730000000000,
  "profile": "quick",                       // "quick" | "full"
  "runtimes": [
    { "kind": "sys",  "program": "/nix/.../erl", "version": "29.1.1", "supported": true,  "notes": [] },
    { "kind": "rust", "program": ".../target/release/beam_rt", "version": "30", "supported": true, "notes": [] }
  ],
  "results": [
    {
      "workload": "baseline",
      "runtime": "sys",
      "category": "startup",
      "description": "system-ERTS startup latency + fixed compute loop",
      "status": "ok",                       // "ok" | "skipped" | "failed"
      "reason": null,                       // skip reason when status == "skipped"
      "metrics": [
        { "name": "startup_ms",      "unit": "ms",    "summary": { "count": 10, "min": 108.0, "max": 210.0, "mean": 131.4, "p50": 124.0, "p90": 180.0, "p99": 205.0 } },
        { "name": "compute_loop_ms", "unit": "ms",    "summary": { "count": 5,  "min": 90.0,  "max": 96.0,  "mean": 92.0,  "p50": 92.0,  "p90": 95.0,  "p99": 96.0 } },
        { "name": "compute_ops_per_sec", "unit": "ops/s", "value": 10870000.0 }
      ],
      "peak_memory_bytes": 34000000,        // null when unmeasurable (see Constraints)
      "notes": []
    }
  ]
}
```
`Metric` = `{ name, unit, value? | summary? }` (exactly one of `value`/`summary`); `Summary` =
`{ count, min, max, mean, p50, p90, p99 }` (all f64 except `count: usize`).

## Library modules (routing table)
- `src/lib.rs` → crate docs, module wiring, `#![forbid(unsafe_code)]`, root re-exports.
- `src/main.rs` → thin binary entry: parse args, dispatch, map result to exit code.
- `src/cli.rs` → hand-rolled arg parser, `Command` enum, usage/help text, `UsageError` (incl. `--out-file`).
- `src/error.rs` → crate error/`Result` type (`BenchError`: usage/env/IO/serde), `From<io::Error>`.
- `src/stats.rs` → `Summary`, `summarize(&[f64]) -> Summary`, `percentile(sorted, p)`, `mean`.
- `src/metrics.rs` → `Metric`, `Metric::scalar`/`Metric::samples`, `Status`, `Category`, `Probe`, `Runtimes`.
- `src/runtime.rs` → `RuntimeKind`, `Runtime`, oracle/rust resolution, `self_check`, `RuntimeInfo`.
- `src/proc.rs` → subprocess helpers: timed run with stdout/stderr capture, `VmHWM` peak-memory poller.
- `src/scratch.rs` → **read-only target adapter**: `ScratchDir` (RAII `$TMPDIR` temp dir), `copy_tree`, `copy_files`.
- `src/http_load.rs` → dependency-free in-process HTTP/1.1 load driver (`LoadConfig`, `drive`, `LoadOutcome`).
- `src/report.rs` → `Report`, `WorkloadResult`, `SCHEMA_VERSION`, JSON save/load (serde).
- `src/compare.rs` → diff two `Report`s into per-(workload,runtime,metric) deltas; one-path mode diffs `sys` vs `rust` within a single report.
- `src/run.rs` → runner: resolve runtimes, iterate (runtime × workload), assemble `Report`, write JSON.
- `src/workload.rs` → the `Workload` trait, `RunContext`, `registry()`, `find()`.
- `src/workloads/mod.rs` → submodule wiring; assembles the workload registry; shared repo/file helpers.
- `src/workloads/tool.rs` → shared external-tool resolution (`command_for`, `nix_shell`, `nix_opt_in`, `REPO_SKIP_NAMES`).
- `src/workloads/baseline.rs` · `erlc_compiler.rs` · `elixir_compiler.rs` · `plb.rs` · `cowboy.rs` ·
  `khepri.rs` · `emqx.rs` · `slice.rs` · `foreign.rs` → one file per workload (probe + run).
  `slice.rs` also exposes `pub(crate) resolve_erlc()` (the shared `$ERLC`/`PATH` lookup reused by
  `foreign.rs`).
- `tests/integration.rs` → hermetic synthetic-report round-trip + `compare` tests (no oracle needed).
- `tests/oracle_smoke.rs` → `baseline` smoke test against the real oracle (no-op skip when unresolved).
- `RESULTS.md` → the definitive results summary (tables, commands, verified skips, host-load record, caveats).
- `results/` → committed raw `Report` JSON + `bench compare` diffs: `m4-*` = the current run (the primary
  full run, the three focused `xrun` runs, the compiler/cowboy/khepri runs, the PLB attempt, and the three
  compare diffs).

## Constraints
- `#![forbid(unsafe_code)]`; **no new crates** (deps resolve offline: `serde`, `serde_json`, `beam_common`).
- External-repo roots are **documented defaults**, overridable via `--repo <id>=<path>` (repeatable)
  and env `BENCH_<ID>_REPO` (e.g. `BENCH_COWBOY_REPO`); no other absolute path is baked in.
- **Never write into the external repos.** Any build/run is staged into a `ScratchDir` copy first. Never
  fabricate a metric.
- **Peak memory:** Linux `/proc/<pid>/status` `VmHWM` polling (pure `std::fs`, no `unsafe`) while the
  child runs; if unreadable ⇒ `peak_memory_bytes: null` + a `notes` entry. Erlang-reported
  `erlang:memory(total)` may be recorded as a *separate* named metric, never conflated with OS RSS.
- Files target ~1000 lines; split by responsibility before growing past it.

## Known Issues / Notes for Agents
- **Spawn goes through a bounded `ETXTBSY` retry** (`proc::spawn_with_retry`, mirroring the
  differential harness): both `proc::run` and `proc::spawn` retry ONLY the transient
  `ExecutableFileBusy` / raw-errno-26 fork/exec race (≤5 attempts, ~10 ms backoff) and return every
  other error kind unchanged. It is a bounded retry of a proven scheduling artifact (a concurrent
  `fork()` while another thread holds a freshly written script open *for writing*), NOT error
  swallowing — do not remove it. `Runtime::self_check_result` surfaces the errors that outlive it
  (`Ok(false)` only for unresolved / `NotFound` / wrong-output; genuine spawn errors are `Err`), and
  `Runtime::self_check() -> bool` stays a thin `unwrap_or(false)` wrapper for `info()`/`oracle_smoke`.
- Build/test via the wrapper (read-only `$HOME`): from the repo root
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench`; a real run:
  `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -p bench -- run --quick --out-file erts_rust/tools/bench/out/target-bench.json`.
  Never hardcode the worktree path.
- Toolchain in this sandbox: `erl`/`erlc`/`escript`/`make`/`mix`/`elixir`/`curl`/`git`/`nix`/`jq` **present**;
  `rebar3`, `dotnet`, `wrk`, `h2load`, `bombardier`, `ab`, `/usr/bin/time` **absent from `PATH`**.
  `rebar3` **3.27.1** and `dotnet` **9.0.318** are both built in the Nix store; prepending their `bin`
  dirs to `PATH` for one invocation (a plain `PATH=` prefix, no env-var opt-in, no repo change) is enough
  for the harness's `PATH` probe to resolve them — this is how `khepri` runs and how the PLB attempt gets
  past the SDK check (PLB still yields no number — see the skip reasons above). Network is available
  (erlang.mk can git-fetch cowlib/ranch). Oracle is OTP 29.1.1 / erts-17.1. After
  `scripts/cargo.sh build --release --workspace`, `../../target/release/beam_rt` is built ⇒ the `rust`
  runtime resolves and passes the self-check (`supported: true`).
- **`$BENCH_TRY_NIX`** (opt-in, default off): when set, `khepri`/`emqx`/`plb` may resolve `rebar3`/`dotnet`
  via `nix shell nixpkgs#<attr> --command …` instead of a PATH lookup. Without it, PATH-only ⇒ honest
  `skipped(<tool>-absent)` unless the store `bin` dir is on `PATH` (see the toolchain bullet above).
- **Cowboy** builds a relx release in the scratch copy (network fetch of cowlib+ranch); the release
  start script `foreground` **`exec`s** `erlexec`, so killing the spawned child stops the VM and its pid
  is the Erlang VM (so `VmHWM` polling works).
- **The benchmark host is shared and usually loaded.** Absolute latencies/throughputs move several-fold
  with load, because the oracle `erl` (a 14-scheduler SMP VM) loses much more to CPU contention than
  `beam_rt`. Prefer the **quiet** primary run and ratios over absolute values, always record
  `/proc/loadavg` with the numbers, and never quote a single contended run as a constant (the recorded
  loads are in `RESULTS.md` §1/§6).
- **Elixir**: the in-repo checkout (`/home/bill/Source/elixir`, `1.21.0-dev`) is unbuilt; the workload
  measures the installed system `elixirc` (1.20.4). Compiling the checkout's stdlib in one `elixirc` batch
  fails on version skew, hence the per-file fallback.
- The external-repo CONTEXT.md files (added read-only, no source changes) record each repo's build/bench
  entry points; consult them via `--repo` overrides.
- **No self-hosted build exists in this crate.** Contract **A.9** is *declared* on `erlc_compiler`
  (`erlc_compiler.rs` `contracts() -> &["A.9"]`) and `elixir_compiler` (`["A.1","A.9"]`), but **both run
  only the system oracle** (`runtimes() -> Runtimes::SysOnly`): `erlc_compiler` shells out to the system
  `erlc` (`erlc_program()`/`proc::which("erlc")`), `elixir_compiler` to the installed `elixirc`. Nothing
  in the driver runs `compile:file/*`, `erlc`, `rebar3`, or any compiler **under `beam_rt`** — so the
  crate exercises no compiler *under the Rust runtime* and no genuine self-hosted build. (The
  `erlc_compiler.rs` module doc line "wall time for both runtimes (oracle and `beam_rt`)" is stale/wrong
  w.r.t. the `SysOnly` gate.) Genuine end-to-end `beam_rt` execution is confined to `baseline`, `slice`,
  and `foreign`; the full target apps (Cowboy release, EMQX, Khepri, Elixir) are all `SysOnly` and report
  `skipped(runtime-unsupported)` under `rust`.
- `out/` is gitignored by `../../.gitignore` (`/tools/bench/out/`); the standard artifact is
  `out/target-bench.json` (via `--out-file`). Durable, reviewable results go to the **tracked**
  `results/` directory (see `RESULTS.md`), which is for committed milestone numbers only — scratch
  runs stay in `out/`.

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `stats` (percentile/mean edge cases), `metrics`/`report`
  (JSON round-trip + schema stability + exactly-one-of value/summary), `cli` (every subcommand, defaults,
  `--help`, `--out-file`, error cases), `probe` decision helpers, `compare` (deltas), `scratch`
  (RAII/copy/skip), `http_load` (loopback keep-alive server + pure parsers), `workloads::tool` (tool/argv),
  `workloads::plb` (a run with no result JSON is a *failed* row, never `ok`).
- Library integration test: build a synthetic in-memory `Report`, serialize, reload, and compare —
  hermetic, no oracle needed.
- Oracle-dependent smoke test (e.g. `baseline --quick`) runs only when `erl`/`$ERTS_ORACLE` resolves,
  and is a no-op skip otherwise, so `cargo test -p bench` stays green in any environment.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` GREEN and
  `... test --workspace` GREEN, plus a demonstrated `run --quick` producing real numbers for
  `baseline`/`erlc_compiler`/`elixir_compiler`/`cowboy`/`foreign` and honest `skipped(reason)` rows
  for the rest. The milestone gate is the `--full` run archived in `results/` and summarised in
  `RESULTS.md`, which must be refreshed whenever the numbers change.

## Runbook — reproducing the sys-vs-rust numbers

Nothing below is required for a code review; it is how the `results/` artifacts and `RESULTS.md` were
produced. Build both the driver and the Rust runtime once, then run the workloads:

```sh
# 1. build the driver AND the runtime (from the repo root; the wrapper supplies a writable CARGO_HOME)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p bench -p beam_rt

# 2. the FULL report (--full, BOTH runtimes, all 9 workloads) — ~4-10 min (host-load dependent)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full.json --verbose

# 3. the FOCUSED cross-runtime report — the reliable source for sys-vs-rust tables (~2 min):
#    only the three cross-runtime workloads, so both sides see the same machine conditions
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign \
    --out-file erts_rust/tools/bench/out/xrun-full.json --verbose

# 4. the toolchain workloads (rebar3 / dotnet are built in the Nix store here — prepend their bin dirs)
PATH=/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin:$PATH \
  erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads khepri,emqx \
    --out-file erts_rust/tools/bench/out/khepri-emqx.json --verbose
# (the $BENCH_TRY_NIX opt-in reaches the same rebar3 via `nix shell nixpkgs#rebar3 --command …`)

# 5. compare (cross-runtime inside one report, or two files)
erts_rust/target/release/bench compare erts_rust/tools/bench/out/xrun-full.json
erts_rust/target/release/bench compare erts_rust/tools/bench/out/xrun-full.json \
                                       erts_rust/tools/bench/out/xrun-full-2.json
```

`foreign` runs under **both** runtimes; every other repo workload is `sys`-only, so a `--runtime all`
report shows a genuine sys-vs-rust row only for `baseline`, `slice`, and `foreign`. Read the numbers as
*reproducible on this host*, not milestone results (single host, finite sample counts).

> **Always measure on a quiet host.** The benchmark host is shared and the absolute latencies move
> several-fold with its load: the quiet primary run is the headline and the contended `xrun` runs are the
> error bar. Check `/proc/loadavg` before and after a run and record it in `RESULTS.md` (which pins the
> observed loads).

**Skip reasons, re-verified on this host (2026):**
- `plb` → `skipped(dotnet-absent)` with a bare `PATH`; **`failed(plb-tool-failed)`** once a `dotnet` is on
  `PATH`. The Nix store holds a .NET 9 SDK (`/nix/store/g6lfw3lizv0ayzy7yhq5g0rk9swxysii-dotnet-sdk-wrapped-9.0.318/bin/dotnet`,
  `9.0.318`); with it on `PATH` PLB's tool builds, but `BenchTool --task bench` requires a prior
  `--task build` stage and throws `System.IO.DirectoryNotFoundException …/bench/build/elixir_linux_elixir_latest_default_*`
  (`Program.cs:617`) because that build output does not exist — the build stage needs a docker/`mix local.hex`
  toolchain with a writable home (`$HOME` is read-only here), which this workload does not drive. **No PLB
  number is reported.** PLB *does* carry Elixir `.ex` sources (`bench/algorithm/*/1.ex`), so a
  dotnet-free `elixir`/escript path is conceivable but unimplemented (and the Rust runtime has no Elixir front end).
- `emqx` → `skipped(not-a-rebar3-project)`: EMQX has no root `rebar.config` (it builds via
  `make`/`mix`), so `rebar3 compile` would be a silent no-op. Verified with `rebar3` actually resolvable
  (store `bin` dir on `PATH`); without it the earlier `skipped(rebar3-absent)` fires first.
- `khepri` → **runs** once the store `rebar3` 3.27.1 `bin` dir is on `PATH`; without it,
  `skipped(rebar3-absent)` — `rebar3` is not on `PATH` by default.

**Regression guard:** `tests/integration.rs::a_real_foreign_workload_is_measured_under_both_runtimes`
asserts the `foreign` workload stays `Runtimes::Both` and keeps declaring contracts A.1/A.2, so the
cross-runtime Rust coverage cannot silently disappear. `foreign.rs`'s unit tests pin the case table
(sources present, calibrated `N`, `N_sys > N_rust`), the per-side argv, and the startup-cancelling
per-call math.

## Measured — where the numbers live

**`RESULTS.md` is the authoritative source for every measured number** (one table per workload category,
the environment record, the exact reproduction commands, the `/proc/loadavg` record, the stability bounds
and a "what these numbers do and do not mean" section).
The raw harness JSON behind it is committed under `results/` as `m4-*`: the **primary full run**
(`m4-bench-full-runtimes-all.json`), the three focused cross-runtime runs, the compiler/cowboy/khepri
sys-only runs, the PLB attempt, and the three `bench compare` diffs.
This file deliberately restates none of those numbers — read them there, and regenerate with the Runbook
above rather than quoting a stale copy.

Structural facts that are not numbers (and so belong here):
- `--runtime all` yields a genuine sys-vs-rust row only for the three `Runtimes::Both` workloads
  (`baseline`, `slice`, `foreign`); every other workload is `SysOnly` (see the table above).
- `baseline`'s compute loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1`, so its
  near-parity result reads as the **native floor**, not interpreter throughput.
- `slice` runs one call per process and `foreign` many calls per process at a per-runtime loop count `N`,
  so peak RSS is like-for-like only for `slice`.
- The execution-bound signal is `foreign`'s per-call cost, not `slice`'s wall times (every `slice` case is a
  single-call program dominated by fixed startup).

> **`ERTS_STDLIB_DIR` note (a real `beam_rt` behavior):** the Rust runtime resolves its system code path
> by spawning `erl` once per OS process unless `ERTS_STDLIB_DIR` is pre-set, so that spawn is inside the
> `_startup_ms` metric (and the `baseline`/`slice`/`foreign` walls). `bench` does **not** set it — the
> harness is a pure driver/observer and never mutates the runtime's environment beyond argv — and the
> per-call metric cancels the cost.

## Reading the current sys-vs-rust per-call gap (verified)

The `foreign` per-call gap (its current value is in `RESULTS.md` §3.4) is a **real runtime cost, not a
harness artifact**. The startup-cancelled metric `(mean wall@N − mean wall@0)/N` reproduces with a plain
shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>` — so CLI re-entry, per-call module
reloads and per-call GC are all ruled out. It is a `beam_rt` host-context cost: `beam_engine`'s interpreter
dispatch is itself O(1) and allocation-free per instruction, and the runtime-side dispatch costs behind it
are `beam_rt` measurements owned by `../../crates/beam_rt/CONTEXT.md` (measured via that crate's
`examples/perf_bif.rs`), not `bench` output.

Consequence for reading results: startup latency is the trustworthy cross-runtime comparison today (and, on
`slice`, peak RSS; `foreign` peak RSS is not — see the Known Issue). The `foreign` rust `ns_per_call` rows — and therefore their `wall_ms` rows — are snapshots of
this per-call cost; re-run the Runbook commands rather than adjusting the numbers.

## Known Issue — rust `foreign` peak RSS scales with the loop count `N`

`beam_rt` does not yet drive a garbage collector, so a looping program accumulates garbage in proportion
to its iteration count. Because the `foreign` rust cases run at large `N`, this drives the rust per-case
`case_<name>_peak_rss_bytes` **above** the sys figures on the same cases and the row moves with the
calibration (same-`N` memory is unchanged; the figures are in `RESULTS.md`). `foreign` peak RSS is
therefore **not** a like-for-like memory comparison — `slice` (one call per process, `N`=1) is the
comparable per-process memory metric.

## See Also
- `RESULTS.md` → the current results summary (one table per workload category, exact reproduction
  commands, verified skip reasons, host-load record, stability, and "what these numbers mean").
- `results/` → the raw harness JSON behind `RESULTS.md` (committed, so the numbers stay reviewable)
  plus the three `bench compare` diffs (`m4-*`).
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy, the differential gate, staging.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the concrete target workloads + build/bench entry points.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I → the mandated success metrics (startup, p99, throughput, memory).
