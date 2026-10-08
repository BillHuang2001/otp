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
| `baseline` | startup | oracle `erl` | **runs** (sys+rust) | `beam_rt` `erlang:memory/0,1` probe unsupported ⇒ an explicit `not-run` note on the rust memory probe (the compute loop itself runs on both) |
| `erlc_compiler` | compiler | `erlc` over OTP stdlib **+** Elixir `lib/elixir/src` `.erl` sources | **runs** (quick+full) | `erlc-absent`, `corpus-missing` |
| `elixir_compiler` | compiler | system `elixirc` over a staged `.ex` corpus from the Elixir checkout | **runs** (quick+full) | `elixirc-absent`, `corpus-missing`, `repo-missing` |
| `plb` | compute | `/home/bill/Source/Programming-Language-Benchmarks` (`dotnet` driver) | skipped / `failed` | `dotnet-absent`, `repo-missing`, `plb-tool-failed`, `plb-no-results` |
| `cowboy` | io | `/home/bill/Source/cowboy` hello_world built in a scratch copy, driven by the in-process loader | **runs** | `repo-missing`, `make-absent`, `deps-unavailable` |
| `khepri` | gc | `/home/bill/Source/khepri` (`rebar3 eunit`, in a scratch copy) | **runs** (with `$BENCH_TRY_NIX`) | `rebar3-absent`, `repo-missing` |
| `emqx` | messaging | `/home/bill/Source/emqx` (`rebar3 compile`, in a scratch copy) | skipped | `rebar3-absent`, `not-a-rebar3-project`, `repo-missing` |
| `slice` | compute | committed `slice.beam` fixture + 6 `erlc`-compiled stdlib wrappers, run under **each scheduled runtime** | **runs** (sys+rust) | `erlc-absent`, `fixture-missing`, `runtime-unsupported` |
| `foreign` | compute | **real Cowboy + EMQX modules** (`cowboy_bstr.erl`, `emqx_base62.erl`) compiled with the system `erlc`, run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `sources-missing`, `runtime-unsupported` |

- `baseline` (always-on): N runs of `<erl> -noshell -eval 'halt().'` ⇒ startup-latency `Summary` (p50/p99);
  a fixed in-VM compute loop `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'`
  ⇒ `compute_loop_ms` + `compute_ops_per_sec`; and a **separate** VM-memory probe
  `-eval 'M = erlang:memory(total), io:format("~p~n",[M]), halt().'` ⇒ the scalar `vm_memory_total_bytes`
  (never conflated with the OS `peak_memory_bytes`, which is the compute runs' RSS). `--quick` uses 10
  startup runs / 5 compute runs; `--full` 50 / 5. **Capability gaps degrade, they never abort:** `beam_rt`
  does not implement `erlang:memory/0,1` (it reports a bare `beam_rt: error undef`, exit 1), so its memory
  probe records **no** metric and the explicit note `vm_memory_total_bytes: not-run (unsupported:
  erlang:memory/1)`; the compute loop, the startup summary and every other metric are still produced. A
  genuine (non-capability-gap) probe failure is surfaced verbatim, never fabricated.
- `erlc_compiler`: compiles a bounded deterministic slice (first 30 `.erl` of each corpus) of the in-repo
  OTP `lib/stdlib/src` **and** the Elixir checkout's `lib/elixir/src` (both read-only) with `erlc` into a
  `ScratchDir` ⇒ files/sec, LOC/sec, wall time, peak memory. Missing/failing Elixir corpus falls back to
  OTP-only with a note.
- `elixir_compiler`: measures the **installed system `elixirc`** (the in-repo checkout is unbuilt/read-only)
  over a fixed `.ex` corpus staged into a `ScratchDir` ⇒ wall time, files/sec, LOC/sec, compiler-startup,
  peak memory. Tries one batch `elixirc`; on failure/short-output falls back to per-file compilation
  (robust to the checkout↔installed version skew) and notes which mode ran.
- `cowboy`: `copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk
  git-fetches cowlib/ranch), starts `_rel/hello_world_example/bin/hello_world_example foreground` on port
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
  ~±30 ms startup noise) and the program is run twice per sample — at `N` and at `0` iterations — giving
  `case_<name>_wall_ms`, `case_<name>_startup_ms`, `case_<name>_peak_rss_bytes`, and the throughput
  metric `case_<name>_ns_per_call = (mean(wall@N) − mean(wall@0)) / N × 1e6`, emitted only when positive
  (else an honest note). Every runtime's stdout must equal `N × per_call` (the driver's accumulator is
  linear in `N`), recorded in an `observed stdout:` note; a Rust exit `3` records the capability gap and
  emits **no** timing. All four cases run under both runtimes today.

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
- `RESULTS.md` → the definitive `--full` results summary (tables, commands, verified skips, caveats).
- `results/` → committed raw `Report` JSON from the milestone run + the `bench compare` diffs.

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
  `rebar3`, `dotnet`, `wrk`, `h2load`, `bombardier`, `ab`, `/usr/bin/time` **absent from `PATH`**
  (`rebar3` 3.27.1 *is* obtainable via the `nix` opt-in; `dotnet` is not usable for PLB — see the skip
  reasons above). Network is available (erlang.mk can git-fetch cowlib/ranch). Oracle is OTP 29.1.1 /
  erts-17.1. After `scripts/cargo.sh build --release --workspace`, `../../target/release/beam_rt` is
  built ⇒ the `rust` runtime resolves and passes the self-check (`supported: true`).
- **`$BENCH_TRY_NIX`** (opt-in, default off): when set, `khepri`/`emqx`/`plb` may resolve `rebar3`/`dotnet`
  via `nix shell nixpkgs#<attr> --command …` instead of a PATH lookup. Without it, PATH-only ⇒ honest
  `skipped(<tool>-absent)`.
- **Cowboy** builds a relx release in the scratch copy (~40 s, network fetch of cowlib+ranch); the release
  start script `foreground` **`exec`s** `erlexec`, so killing the spawned child stops the VM and its pid
  is the Erlang VM (so `VmHWM` polling works).
- **Elixir**: the in-repo checkout (`/home/bill/Source/elixir`, `1.21.0-dev`) is unbuilt; the workload
  measures the installed system `elixirc` (1.20.4). Compiling the checkout's stdlib in one `elixirc` batch
  fails on version skew, hence the per-file fallback.
- The external-repo CONTEXT.md files (added read-only, no source changes) record each repo's build/bench
  entry points; consult them via `--repo` overrides.
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
# 1. build (from the repo root; the committed wrapper supplies a writable CARGO_HOME)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p bench -p beam_rt

# 2. the primary milestone report (--full, BOTH runtimes, all workloads) — ~4 min
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full.json --verbose

# 3. the opt-in nix toolchain workloads (rebar3 present only via the Nix store here) — ~80 s
BENCH_TRY_NIX=1 erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads khepri,emqx \
    --out-file erts_rust/tools/bench/out/nix-full.json --verbose

# 4. compare (cross-runtime inside one report, or two files)
erts_rust/target/release/bench compare erts_rust/tools/bench/out/milestone-full.json
erts_rust/target/release/bench compare erts_rust/tools/bench/out/milestone-full.json \
                                       erts_rust/tools/bench/out/milestone-full-2.json
```

`foreign` runs under **both** runtimes; every other repo workload is `sys`-only, so a `--runtime all`
report shows a genuine sys-vs-rust row only for `baseline`, `slice`, and `foreign`. Read the numbers as
*reproducible on this host*, not milestone results (single host, finite sample counts).

**Skip reasons, re-verified on this host (2026):**
- `plb` → `skipped(dotnet-absent)`: `dotnet` is on neither `PATH` nor in the Nix store
  (`ls -d /nix/store/*dotnet*` → 0 built outputs). With `BENCH_TRY_NIX=1` the `nixpkgs#dotnet-sdk`
  fallback *does* resolve (8.0.425, ~20 s from `cache.nixos.org`) but **cannot build PLB's tool**:
  `bench/tool/BenchTool.csproj` declares `<TargetFramework>net9</TargetFramework>` ⇒
  `error NETSDK1045`, `dotnet` exits `1` (verified) ⇒ the row is `failed(plb-tool-failed)`.
  A .NET 9 SDK (`nixpkgs#dotnet-sdk_9`, 9.0.318) *does* build the tool, but PLB's `--task bench` then
  needs a prior `--task build` stage plus a docker/`mix local.hex` toolchain with a writable home
  (`$HOME` is read-only here), which this workload does not drive. PLB *does* carry Elixir `.ex`
  sources (`bench/algorithm/*/1.ex`), so a dotnet-free `elixir`/escript path is conceivable but
  unimplemented (and the Rust runtime has no Elixir front end).
- `emqx` → `skipped(not-a-rebar3-project)`: EMQX has no root `rebar.config` (it builds via
  `make`/`mix`), so `rebar3 compile` would be a silent no-op. Verified with `rebar3` actually
  resolvable (`BENCH_TRY_NIX=1`); without the opt-in the earlier `skipped(rebar3-absent)` fires first.
- `khepri` → **runs** with `BENCH_TRY_NIX=1` (`rebar3` 3.27.1 resolves from the Nix store); without it,
  `skipped(rebar3-absent)` — `rebar3` is not on `PATH`.

**Regression guard:** `tests/integration.rs::a_real_foreign_workload_is_measured_under_both_runtimes`
asserts the `foreign` workload stays `Runtimes::Both` and keeps declaring contracts A.1/A.2, so the
cross-runtime Rust coverage cannot silently disappear. `foreign.rs`'s unit tests pin the case table
(sources present, calibrated `N`, `N_sys > N_rust`), the per-side argv, and the startup-cancelling
per-call math.

## Measured — the definitive `--full` verification run

The milestone numbers live in **`RESULTS.md`** (one table per workload category) and are archived as
raw harness JSON under **`results/`** (`bench-full-runtimes-all.json` = the primary run,
`…-run2.json` = an identical repetition, `bench-full-nix-khepri-emqx.json`, the `plb` attempt, plus two
`bench compare` diffs). `RESULTS.md` also carries the full environment record, the exact commands and a
"what these numbers do and do not mean" section.

Reference host: NixOS 26.11, `x86_64`, 14 CPUs (Intel Core Ultra 7 356H), 62.3 GiB RAM;
oracle = system `erl` **OTP 29.1.1 / erts-17.1**, rust = `erts_rust/target/release/beam_rt`
(`otp_release` = `30`) — both resolve and pass the self-check. `elixirc` = Elixir 1.20.4;
`rebar3` = 3.27.1 (only via `nix shell`, i.e. `BENCH_TRY_NIX=1`); `dotnet` absent from `PATH` **and**
from the Nix store; `$HOME` read-only. `--full` sample counts: `baseline` startup n=50 / compute n=5;
`slice` n=25 per case; `foreign` n=10 per case; `cowboy` 16 connections × 5 s (20 000 latency samples).

| workload | runtime | headline (`--full`) |
|---|---|---|
| `baseline` | sys | startup p50 **118.31 ms** / p99 145.39 ms; compute loop 190.48 ms p50 ⇒ **5.46 M ops/s**; `erlang:memory(total)` 55.3 MB; peak RSS 124.5 MiB |
| `baseline` | rust | startup p50 **150.86 ms** / p99 172.29 ms (≈ 1.28× sys). The **archived M1** report's compute row is "not run"; the harness now measures the compute loop on both runtimes and only the `erlang:memory/1` probe is a capability gap — see `RESULTS.md` §3.2 |
| `erlc_compiler` | sys | **9.45 files/s** (7 327 loc/s), 60 files, peak RSS 128.6 MiB |
| `elixir_compiler` | sys | **4.86 files/s** (2 438 loc/s), 105 attempted → 223 `.beam` via the per-file fallback, peak RSS 202.5 MiB |
| `cowboy` | sys | **218 788 req/s**, latency p50 0.063 ms / p99 0.387 ms, 0 errors in 1 094 402 req, peak RSS 153.4 MiB |
| `khepri` | sys | `rebar3 eunit` **76.9 s**, peak RSS 349.0 MiB (`BENCH_TRY_NIX=1`) |
| `slice` | sys vs rust | 9/9 identical stdout; wall **~parity** (1.11–1.32×) because every case is startup-dominated; peak RSS 68.3 vs **36.7 MiB** |
| `foreign` | sys vs rust | per-call cost **703×–9 891×** higher under rust; startup ~parity (~130 ms vs ~150 ms); peak RSS 68.7 vs **27.8 MiB** |
| `plb`, `emqx` | sys | not run — verified reasons in `RESULTS.md` §5 |

**The execution-bound result is the `foreign` per-call gap, not the `slice` wall times**: `slice` cases
are single-call programs whose wall time is dominated by the fixed startup cost below. Run-to-run
stability (Run 1 vs Run 2, 88 numeric rows, `results/compare-run1-vs-run2.txt`): everything within a
few percent except `foreign` rust `ns_per_call` (up to **+35 %**) and `slice` rust `string_str_length`
(**+48 %**) — single-digit-percent figures reproduce, two-digit ones do not.

> **`ERTS_STDLIB_DIR` note (a real `beam_rt` behavior, re-measured on this host):** the Rust runtime
> resolves the system code path by spawning `erl` once per process, so `beam_rt -noshell -eval 'halt().'`
> costs **139–156 ms** (5 runs) bare versus **13–20 ms** (5 runs) with
> `ERTS_STDLIB_DIR=<code:lib_dir(stdlib)>` pre-set. `bench` does **not** set it — the harness is a pure
> driver/observer and never mutates the runtime's environment beyond argv — and the effect is confined
> to the `_startup_ms` metric, which the per-call metric cancels.

## See Also
- `RESULTS.md` → the definitive `--full` results summary (one table per workload category, exact
  reproduction commands, verified skip reasons, stability, and "what these numbers do and do not mean").
- `results/` → the raw harness JSON behind `RESULTS.md` (committed, so the numbers stay reviewable)
  plus the two `bench compare` diffs.
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy, the differential gate, staging.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the concrete target workloads + build/bench entry points.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I → the mandated success metrics (startup, p99, throughput, memory).
