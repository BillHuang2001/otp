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

### `beam_rt` is a STUB (caveat)
The Rust runtime binary does not exist / is a placeholder today. It is resolved, then **self-checked**: a
runtime is *supported* only if it can execute the canonical probe
`-noshell -eval 'io:format("~p~n",[1+1]), halt().'` and print `2`. The stub fails this ⇒ every
rust-runtime result is reported `skipped(runtime-unsupported)`, **never** a fake number.

## API Surface

### CLI (hand-rolled; usable `--help` on every subcommand)
- `bench list [--json]` — list workload ids, category, contract coverage, and current probe status.
- `bench run [--quick|--full] [--workloads <ids,csv>] [--runtime <sys|rust|all>] [--out <dir>] [--out-file <path>] [--repo <id>=<path>] [--json] [--verbose]`
- `bench baseline [--quick|--full] [--json] [--out <dir>] [--out-file <path>]` — measure **only** the system-ERTS baseline.
- `bench compare <a.json> <b.json> [--json]` — diff two result sets.
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
| `baseline` | startup | oracle `erl` | **runs** (sys) | rust ⇒ `runtime-unsupported` |
| `erlc_compiler` | compiler | `erlc` over OTP stdlib **+** Elixir `lib/elixir/src` `.erl` sources | **runs** (quick+full) | `erlc-absent`, `corpus-missing` |
| `elixir_compiler` | compiler | system `elixirc` over a staged `.ex` corpus from the Elixir checkout | **runs** (quick+full) | `elixirc-absent`, `corpus-missing`, `repo-missing` |
| `plb` | compute | `/home/bill/Source/Programming-Language-Benchmarks` (`dotnet` driver) | skipped | `dotnet-absent`, `repo-missing` |
| `cowboy` | io | `/home/bill/Source/cowboy` hello_world built in a scratch copy, driven by the in-process loader | **runs** | `repo-missing`, `make-absent`, `deps-unavailable` |
| `khepri` | gc | `/home/bill/Source/khepri` (`rebar3 eunit`, in a scratch copy) | skipped | `rebar3-absent`, `repo-missing` |
| `emqx` | messaging | `/home/bill/Source/emqx` (`rebar3 compile`, in a scratch copy) | skipped | `rebar3-absent`, `repo-missing` |

- `baseline` (always-on): N runs of `<erl> -noshell -eval 'halt().'` ⇒ startup-latency `Summary`
  (p50/p99), plus a fixed in-VM compute loop (`-eval` summing 1..1_000_000) ⇒ wall time + throughput, so
  a baseline record always exists. `--quick` uses ~10 runs, `--full` ~50.
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
  `emqtt-bench`) are outreach/future work.

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
    { "kind": "rust", "program": null,           "version": null,     "supported": false, "notes": ["beam_rt stub / not built"] }
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
- `src/compare.rs` → diff two `Report`s into per-(workload,runtime,metric) deltas.
- `src/run.rs` → runner: resolve runtimes, iterate (runtime × workload), assemble `Report`, write JSON.
- `src/workload.rs` → the `Workload` trait, `RunContext`, `registry()`, `find()`.
- `src/workloads/mod.rs` → submodule wiring; assembles the workload registry; shared repo/file helpers.
- `src/workloads/tool.rs` → shared external-tool resolution (`command_for`, `nix_shell`, `nix_opt_in`, `REPO_SKIP_NAMES`).
- `src/workloads/baseline.rs` · `erlc_compiler.rs` · `elixir_compiler.rs` · `plb.rs` · `cowboy.rs` ·
  `khepri.rs` · `emqx.rs` → one file per workload (probe + run).
- `tests/integration.rs` → hermetic synthetic-report round-trip + `compare` tests (no oracle needed).
- `tests/oracle_smoke.rs` → `baseline` smoke test against the real oracle (no-op skip when unresolved).

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
- Toolchain in this sandbox: `erl`/`erlc`/`escript`/`make`/`mix`/`elixir`/`curl`/`git`/`nix` **present**;
  `rebar3`, `dotnet`, `wrk`, `h2load`, `bombardier`, `ab`, `/usr/bin/time` **absent**. Network is
  available (erlang.mk can git-fetch cowlib/ranch). Oracle is OTP 29.1.1 / erts-17.1. `../../target/`
  is not built ⇒ `beam_rt` unresolved.
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
  `out/target-bench.json` (via `--out-file`).

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `stats` (percentile/mean edge cases), `metrics`/`report`
  (JSON round-trip + schema stability + exactly-one-of value/summary), `cli` (every subcommand, defaults,
  `--help`, `--out-file`, error cases), `probe` decision helpers, `compare` (deltas), `scratch`
  (RAII/copy/skip), `http_load` (loopback keep-alive server + pure parsers), `workloads::tool` (tool/argv).
- Library integration test: build a synthetic in-memory `Report`, serialize, reload, and compare —
  hermetic, no oracle needed.
- Oracle-dependent smoke test (e.g. `baseline --quick`) runs only when `erl`/`$ERTS_ORACLE` resolves,
  and is a no-op skip otherwise, so `cargo test -p bench` stays green in any environment.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` GREEN and
  `... test --workspace` GREEN, plus a demonstrated `run --quick` producing real numbers for
  `baseline`/`erlc_compiler`/`elixir_compiler`/`cowboy` and honest `skipped(reason)` rows for the rest.

## See Also
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy, the differential gate, staging.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the concrete target workloads + build/bench entry points.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I → the mandated success metrics (startup, p99, throughput, memory).
