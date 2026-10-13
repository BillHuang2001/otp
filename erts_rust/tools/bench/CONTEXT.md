# Benchmark Driver (`erts_rust/tools/bench`) — `bench` crate

## Intent
`bench` is the in-repo **benchmark driver** for the Rust ERTS rewrite. It builds and runs the external
pure-Erlang/Elixir target workloads named in `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` and **always
records the system-ERTS baseline**, capturing the success metrics mandated by
`../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I: **startup latency, p50/p99 tail latency, throughput,
peak memory**.
It is a **library + binary** crate: the binary (`bench`) is what a human or CI drives; the library exists
so `cargo test -p bench` can assert the harness itself (statistics, JSON-schema stability, probe logic,
CLI parsing, report comparison, tool resolution).
`bench` is a **pure driver/observer**: it never modifies the Rust runtime and never writes to the external
repos. Full suites are milestone-only (Part I §C); the default `--quick` path stays bounded.

## Shared conventions (binding — from `../CONTEXT.md`, the `tools/` node)
- **Runtime resolution — documented, overridable, no baked absolute paths:**
  - **Oracle (`sys`)** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud env error (exit 2).
  - **Rust (`rust`)** = `$BEAM_RT` if set, else the workspace-relative `../../target/{release,debug}/beam_rt`
    (resolved from `CARGO_MANIFEST_DIR`).
  - Both are invoked with the **same argv as `erl`** — the runtime binary is treated as a drop-in `erl`
    (contract A.6). Only the *program path* is set; internals are never assumed.
- **Honest reporting:** a case/workload that cannot run is `skipped(<reason>)` / `failed(<reason>)` —
  **never** `pass`, and metrics are never fabricated. Unknown/unavailable ⇒ explicit reason string.
- **Machine-readable results:** JSON with a stable schema (below), written under the gitignored `out/`
  dir (already ignored by `../../.gitignore`: `/tools/bench/out/`) unless `--out` overrides.
  Durable, reviewable milestone results go to the **tracked** `results/` directory (see `RESULTS.md`).
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

### External-tool resolution (`src/workloads/tool.rs`) — no `PATH` prefix, no baked store paths
The tool-driven workloads (`khepri`, `emqx`, `plb`) resolve their external tool (`rebar3`, `mix`/`make`,
`dotnet`) through one shared, documented policy, implemented once:
1. **`PATH`** (`proc::which`) — always consulted live;
2. **`$BENCH_TOOL_PATH`** — colon-separated extra `bin` dirs, empty entries dropped (the developer's override);
3. an **automatic Nix-store scan** for `/nix/store/*<tool>*/bin/<tool>`, ranked **by the version named in
   the entry** (descending) — the random 32-char hash prefix never participates, so the "newest installed"
   preference is real and a too-old version cannot win by accident; **no store hash or version is baked in**;
4. **`$BENCH_TRY_NIX`** (opt-in, default off): a validated `nix shell nixpkgs#<attr> --command <tool>` shim — last, consulted **only** when steps 1–3 find nothing.
Steps 1–3 are subprocess-free and memoised in a process-global cache, so `probe()` / `bench list` stay cheap.
A `None` means the tool is genuinely unavailable ⇒ the caller keeps its honest `skipped(<tool>-absent)`.
`available()` (the `probe()` predicate) returns true when steps 1–3 resolve the tool **or** `$BENCH_TRY_NIX` is set, so `probe_ready` can read true while a later `--version` validation fails; the workload still reports honestly and never fabricates a number.

**Two resolution entry points.** `command_for(tool, nix_attr)` picks the **first** candidate in source order
(steps 1–4 above) — the default for tools with no version constraint.
`command_for_min(tool, nix_attr, min_version: &[u64])` is the **version-aware** path for a tool whose consumer
needs a minimum version (used by `plb`, which declares `MIN_DOTNET_SDK = &[9]` because PLB's envs target
`net9`): it enumerates **every** candidate across `PATH` + `$BENCH_TOOL_PATH` + the store, probes each one's
`--version`, and **selects the greatest version that satisfies `min_version`** — source order is only the
tie-break, so a too-old `dotnet` on `PATH` can no longer shadow a newer store SDK. A candidate whose version
cannot be probed never satisfies. When nothing satisfies it still returns the best available candidate plus an
explicit `no <tool> >= X available; using the best available …` note (never a silent wrong pick). It returns a
`ToolCommand { command, notes }`: the resolved binary + version + source and every rejection reason
(`rejected dotnet 8.0.425 at <path> (<source>): requires >= 9`) go into the workload row's `notes`, in both the
success and the failure branch, so the recorded outcome is auditable.
**Verified in this sandbox:** the store is otherwise subprocess-resolved (`rebar3` 3.27.1, no `PATH` prefix and no `$BENCH_TRY_NIX`), and `dotnet` resolves to **9.0.318** — on `PATH` (`/run/current-system/sw/bin/dotnet`) or, when an older `dotnet` is first on `PATH`, from the store `dotnet-sdk-9.0.318` via `command_for_min` — so `khepri` and `plb` run on this host (§ Known Issues for the exact smoke evidence).
`$BENCH_TRY_NIX` is also verified to work here (it fetches `rebar3` 3.27.1 + Erlang/OTP 28 from `cache.nixos.org`; the network is available), but it stays the last resort described above.
`dotnet` is **store state, not repo state**: PLB needs a ≥ 9.0 SDK, and `command_for_min` finds it wherever it lives (a store entry realized with `nix build --no-link nixpkgs#dotnet-sdk_9`, or a ≥ 9.0 `dotnet` on `PATH`). If no such SDK is reachable, `plb` is either `skipped(dotnet-absent)` or — when an older `dotnet` resolves but none is ≥ 9 — a genuine `failed(plb-build-failed)` with the rejected-candidate notes attached, recorded honestly, never papered over.
**Flake NOTE (do NOT edit `../../flake.nix`):** it lists `rebar3` in `oracleDeps` but no .NET SDK; adding one there would make `plb` reproducible.
The sandbox record of what each tool-driven target actually does here is `results/m9-tool-targets-notes.md`.

### `beam_rt` is a REAL runtime (self-checked)
`beam_rt` is a working Rust runtime (real `-eval` evaluator, a global-atom `code:*`/`init:*` floor, native
`os:cmd/1`); it is resolved from `$BEAM_RT` or the workspace-relative `../../target/{release,debug}/beam_rt`.
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
| `baseline` | startup | oracle `erl` | **runs** (sys+rust) | `runtime-unsupported` |
| `erlc_compiler` | compiler | `erlc` over OTP stdlib **+** Elixir `lib/elixir/src` `.erl` sources | **runs** (quick+full) | `erlc-absent`, `corpus-missing` |
| `elixir_compiler` | compiler | system `elixirc` over a staged `.ex` corpus from the Elixir checkout | **runs** (quick+full) | `elixirc-absent`, `corpus-missing`, `repo-missing` |
| `plb` | compute | `/home/bill/Source/Programming-Language-Benchmarks` (dotnet tool, build+bench; needs a ≥ 9.0 SDK, resolved by `command_for_min`) | **runs** (sys) | `dotnet-absent`, `repo-missing`, `plb-build-failed`, `plb-tool-failed`, `plb-no-results` |
| `cowboy` | io | `/home/bill/Source/cowboy` hello_world built in a scratch copy, driven by the in-process loader | **runs** (sys) | `repo-missing`, `make-absent`, `deps-unavailable` |
| `khepri` | gc | `/home/bill/Source/khepri` (`rebar3 eunit`, in a scratch copy) | **runs** (sys) | `rebar3-absent`, `repo-missing` |
| `emqx` | messaging | `/home/bill/Source/emqx` (real `make`/`mix` build, in a scratch copy) | **runs** on an OTP-28 host; `skipped(emqx-requires-otp-28)` on this OTP-29 host (checkout pins OTP **28.5.0.7-2** at `env.sh:3`/`.tool-versions:1`) | `mix-absent`, `make-absent`, `repo-missing`, `not-an-emqx-project`, `emqx-requires-otp-28`, `emqx-build-failed` |
| `slice` | compute | committed `slice.beam` fixture + stdlib wrapper modules, run under **each scheduled runtime** | **runs** (sys+rust) | `erlc-absent`, `fixture-missing`, `runtime-unsupported` |
| `foreign` | compute | **real Cowboy + EMQX modules** (`cowboy_bstr.erl`, `emqx_base62.erl`) compiled with the system `erlc`, run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `sources-missing`, `runtime-unsupported` |
| `list_walk` | compute | in-tree embedded `bench_list_walk` module (BIF-free recursive `sum/1` list walk), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent` |
| `copy_bound` | messaging | in-tree embedded `bench_copy_bound` module (ETF / ETS / two-process copy round-trips), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `module-compile-failed`, `runtime-unsupported` |
| `gc_bound` | gc | in-tree embedded `bench_gc_bound` module (allocate-and-abandon garbage churn), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `module-compile-failed`, `runtime-unsupported` |

- `baseline` (always-on): N runs of `<erl> -noshell -eval 'halt().'` ⇒ startup-latency `Summary` (p50/p99);
  a fixed in-VM compute loop `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'`
  ⇒ `compute_loop_ms` + `compute_ops_per_sec`; and a **separate** VM-memory probe
  `-eval 'M = erlang:memory(total), io:format("~p~n",[M]), halt().'` ⇒ the scalar `vm_memory_total_bytes`
  (never conflated with the OS `peak_memory_bytes`, which is the compute runs' RSS). `--quick` uses 10
  startup runs / 5 compute runs; `--full` 50 / 5. **Both runtimes produce all three metrics:**
  `beam_rt` implements `erlang:memory/0,1` through the `MemoryHook` seam. **Capability gaps degrade, they
  never abort:** if a runtime does not implement a probed BIF, the probe records **no** metric plus an
  explicit `… : not-run (unsupported: …)` note, while the compute loop, the startup summary and every other
  metric are still produced. A genuine (non-capability-gap) probe failure is surfaced verbatim, never
  fabricated. Note: the loop is dominated by the natively-served `lists:seq/2`+`lists:sum/1`, so a
  near-parity rust figure reflects the native floor, **not** execution throughput (that is
  `foreign`/`list_walk`).
- `erlc_compiler`: compiles a bounded deterministic slice (first 30 `.erl` of each corpus) of the in-repo
  OTP `lib/stdlib/src` **and** the Elixir checkout's `lib/elixir/src` (both read-only) with `erlc` into a
  `ScratchDir` ⇒ files/sec, LOC/sec, wall time, peak memory. Missing/failing Elixir corpus falls back to
  OTP-only with a note.
- `elixir_compiler`: measures the **installed system `elixirc`** (the in-repo checkout is unbuilt/read-only)
  over a fixed `.ex` corpus staged into a `ScratchDir` ⇒ wall time, files/sec, LOC/sec, compiler-startup,
  peak memory. Tries one batch `elixirc`; on failure/short-output falls back to per-file compilation
  (robust to the checkout↔installed version skew) and notes which mode ran.
- `cowboy`: `copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk
  git-fetches cowlib/ranch; a scratch-copy relx build), starts the release `foreground` on port 8080,
  drives it with `http_load::drive` (in-process keep-alive HTTP/1.1), records build/latency/throughput and
  the server's peak RSS, then stops it. Build/dep failure ⇒ `skipped(deps-unavailable)`; compile error ⇒ `failed`.
- `khepri`: stages the Khepri checkout into a `ScratchDir` and runs `rebar3 eunit` there ⇒ wall time + peak
  memory. `rebar3` resolves via the store scan, so it runs with no `PATH` prefix; absent ⇒ `skipped(rebar3-absent)`.
- `emqx`: stages the checkout and drives EMQX's **real** build entry point — the `make`/`mix` umbrella build
  (`mix deps.get` + `mix compile` via the repo's own `./mix`) — in the scratch copy, with `HOME`/`MIX_HOME`/
  `HEX_HOME` redirected into the scratch so the read-only checkout and developer home are untouched. A
  toolchain-matching host yields a real wall time; a mismatched one records the **precise** blocker rather
  than a fabricated number (this host is OTP 29.1.1 / erts-17.1 while EMQX pins **28.5.0.7-2** at
  `env.sh:3`/`.tool-versions:1` ⇒ `skipped(emqx-requires-otp-28)`, the `:ezstd` `env.mk` make-target being
  the mechanism). `mix`/`make` absent ⇒ `mix-absent`/`make-absent`;
  a checkout that is not an EMQX project ⇒ `not-an-emqx-project`.
- `plb`: stages the checkout into a `ScratchDir` and drives the dotnet tool in **two stages**:
  `--task build --langs elixir --no-docker` (copies sources + templates, runs the env's `build:` command,
  moves the result into `bench/build/<build-id>`), then `--task bench --langs elixir` (measures those
  executables), reading `bench/build/_results/**/*.json` (`timeMS`/`memBytes`) from the **scratch** copy.
  The elixir env is declared containerised; with no working container runtime the workload drives it with
  `--no-docker` against the local `mix` toolchain. `HOME`, `DOTNET_CLI_HOME`, `NUGET_PACKAGES`, `MIX_HOME`
  and `TMPDIR` are redirected into the scratch (the sandbox `$HOME` is read-only) so the toolchain can write
  and nothing lands in the read-only source. `dotnet` resolves through the **version-aware** policy
  (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` with `MIN_DOTNET_SDK = &[9]`): it picks the
  greatest `dotnet` ≥ 9.0 across `PATH`/`$BENCH_TOOL_PATH`/the store, so a too-old `dotnet` on `PATH` no longer
  shadows a newer store SDK, and writes the chosen binary + version + source (and every rejection) into the
  row's notes. Honesty: no result JSON ⇒ `failed` (`plb-tool-failed` / `plb-no-results`), a broken build stage ⇒
  `failed(plb-build-failed)` — **never** `ok`, because the only other number on such a row is the tool's own
  wall time.
- `slice`: runs a fixed table of **module-run cases** under whatever runtime it is scheduled on (so under
  `--runtime all` it is a real sys-vs-rust speed + memory comparison of the *same* programs). 3 cases use the
  committed `slice.beam` fixture (`add/2`, `fib/1`, `pick/1`); the rest use tiny embedded stdlib/kernel
  wrapper modules compiled once with the system `erlc` (calling a *real* system module:
  `calendar`/`queue`/`gb_trees`/`base64`/`ordsets`/`string`). `--quick` ⇒ 5 timed runs/case, `--full` ⇒ 25.
  Metrics are per-case summaries `case_<name>_wall_ms` / `case_<name>_peak_rss_bytes` (peak RSS = external
  `VmHWM`); an `observed stdout:` note records the agreement, and a Rust exit `3` (`unsupported: …`) yields a
  note and **no** timing. A `beam_rt` process resolves the system code path with a **native filesystem probe**
  (`crates/beam_rt/src/codeserver.rs`; the `erl` shell-out survives only as a last resort), so the wrapper cases
  no longer pay an `erl` startup — only the cheap path probe when a case first needs a system module.
- `foreign`: **real modules from the foreign target repos, run under *both* runtimes.** Every repo-backed
  workload is `SysOnly` (the Rust runtime cannot boot an OTP release, run `rebar3`, or act as
  `erlc`/`elixirc`), so `foreign` instead picks **pure, self-contained modules from the Cowboy and EMQX
  checkouts** (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), copies the `.erl` sources
  into a `ScratchDir`, compiles them **once** with the system `erlc` together with an embedded driver
  (`bench_foreign_driver`), and runs the identical `.beam` under **whatever runtime it is scheduled on**.
  Measurement is **startup-corrected per call**: each case declares a per-runtime loop count `N` and the
  program is run twice per sample — at `N` and at `0` iterations — giving `case_<name>_wall_ms`,
  `case_<name>_startup_ms`, `case_<name>_peak_rss_bytes`, and the throughput metric
  `case_<name>_ns_per_call = (mean(wall@N) − mean(wall@0)) / N × 1e6`, emitted only when positive (else an
  honest note). Every runtime's stdout must equal `N × per_call` (recorded in an `observed stdout:` note); a
  Rust exit `3` records the capability gap and emits **no** timing. `foreign` peak RSS is **not** a
  like-for-like memory comparison (rust RSS scales with the loop count — see Known Issues); `slice` is.
- `list_walk`: **the permanent, reproducible per-element dispatch-throughput signal** — an in-tree embedded
  `bench_list_walk` module with a **BIF-free, non-tail-recursive `sum/1`** over a fixed-length list, built
  by a pure-Erlang `build/2` recursion (no native `lists:seq/2`), so the whole program exercises only the
  engine's per-element dispatch/return path (~7 dispatched instructions per element, the same shape as
  `crates/beam_engine/tests/engine_bench.rs`; the baseline tier does not inline that recursion).
  `walk(N)` sums the list `N` times and returns the deterministic `N × (LEN×(LEN+1)/2)`; `LEN = 1000` (a
  Rust const formatted into the Erlang source so the two cannot drift). The Rust and oracle sides use
  per-runtime loop counts (`n_rust` = 1 000, `n_sys` = 100 000 ⇒ 10⁶ vs 10⁸ elements). It uses the same
  startup-cancelled recipe as `foreign` (run at `N` and at `0`), reporting `case_list_walk_wall_ms`,
  `case_list_walk_startup_ms`, `case_list_walk_peak_rss_bytes`, `case_list_walk_ns_per_element`
  (`(mean(wall@N) − mean(wall@0)) / (N × LEN) × 1e6`, only when positive) and the **deterministic,
  contention-immune** companion `case_list_walk_elements` = `N × LEN` (stays valid on a loaded host where
  the wall-clock rows do not). Each run's stdout is checked against the deterministic constant; a Rust exit
  `3` records the capability gap and emits **no** timing. `probe()` ⇒ `skipped(erlc-absent)` without `erlc`.
- `copy_bound`: **the messaging / term-copy-bound metric** — the term-copy boundary run under *both* runtimes. One embedded `bench_copy_bound` module (generated from the crate's constants, so Rust and Erlang cannot drift) with three cases, each copying the **same structured payload** (a 5-key map) and validating the round-trip with a bound-variable pattern (a real equality test that raises on a corrupted copy): `etf_roundtrip` (`term_to_binary/1` + `binary_to_term/1` — the External Term Format wire codec, contract A.5); `ets_copy` (`ets:insert/2` + `ets:lookup/2` — the ETS copy boundary, one table+key per process so the measured work is the copy, not table growth); `send_recv` (a term sent to a second process, round-tripped through `term_to_binary/1` there, and sent back — **both directions** of the process copy boundary, with an `after` guard so a missing reply fails loudly, never hangs). It uses the shared startup-cancelled per-op recipe (run at `N` and at `0`, with a per-case per-runtime `N`; `etf_roundtrip` 5 000/1 500 000, `ets_copy` 100 000/2 000 000, `send_recv` 2 000/500 000 rust/sys; `--quick` ⇒ 3 runs/case, `--full` ⇒ 10), reporting `case_<name>_wall_ms`, `case_<name>_startup_ms`, `case_<name>_peak_rss_bytes`, `case_<name>_ns_per_op` (`(mean wall@N − mean wall@0) / N × 1e6`, only when positive) and the deterministic, contention-immune `case_<name>_ops` = `N`. A Rust exit `3` records the capability gap and emits **no** timing; an embedded-module compile failure is `failed(module-compile-failed)` (the source is ours), never a skip; `probe()` ⇒ `skipped(erlc-absent)` via the shared `slice::resolve_erlc()` lookup. **Attribution caveat:** `payload/0` is rebuilt inside every iteration (`src/workloads/copy_bound.rs:240`), and each iteration also runs the bound-variable equality match, so the `etf_roundtrip` per-op figure is the *whole iteration* (payload construction + `term_to_binary`/`binary_to_term` + the structural equality test), **not the codec alone** — the same shape holds for `ets_copy`/`send_recv`, and both runtimes pay the identical per-iteration setup.
- `gc_bound`: **the GC-bound metric** — allocate-and-abandon garbage churn under *both* runtimes. One embedded `bench_gc_bound` module whose single `churn/1` case, per iteration, builds and abandons three `LEN`=256-sized structures — a pure-Erlang recursion list (`build_list/2`, *not* a native `lists:seq/2`), a 256-byte binary built from an Erlang-level char list, and a tuple built from another Erlang list — then checks the two O(1)-checkable ones (`byte_size`/`tuple_size`) so a silently-dropped allocation fails loudly. It uses the same shared startup-cancelled per-op recipe (run at `N` and at `0`; `n_rust` = 2 000, `n_sys` = 200 000; `--quick` ⇒ 3 runs, `--full` ⇒ 10), reporting `case_churn_wall_ms`, `case_churn_startup_ms`, `case_churn_peak_rss_bytes`, `case_churn_ns_per_op` (`(mean wall@N − mean wall@0) / N × 1e6`, only when positive) and the deterministic, contention-immune `case_churn_ops` = `N`. The row measures the allocate-and-abandon wall time and **claims no collection count** (whether a runtime's collector runs during the loop is its own admission policy); a Rust exit `3` ⇒ an honest `skipped(unsupported-case)` with **no** timing, and a compile failure is `failed(module-compile-failed)`; `probe()` ⇒ `skipped(erlc-absent)` via `slice::resolve_erlc()`.

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
- `src/workloads/tool.rs` → shared external-tool resolution (`available`, `command_for` (source order),
  `command_for_min` (version-aware: `Candidate`/`ToolCommand`/`select_candidate`/`probe_tool_version`),
  `extra_tool_dirs`, the Nix-store scan with **version-aware** ranking, `parse_tool_path`, `REPO_SKIP_NAMES`).
- `src/workloads/baseline.rs` · `erlc_compiler.rs` · `elixir_compiler.rs` · `plb.rs` · `cowboy.rs` ·
  `khepri.rs` · `emqx.rs` · `slice.rs` · `foreign.rs` · `list_walk.rs` · `copy_bound.rs` · `gc_bound.rs`
  → one file per workload (probe + run).
  `slice.rs` also exposes `pub(crate) resolve_erlc()` (the shared `$ERLC`/`PATH` lookup reused by
  `foreign.rs`, `list_walk.rs`, `copy_bound.rs` and `gc_bound.rs`).
- `tests/integration.rs` → hermetic synthetic-report round-trip + `compare` tests (no oracle needed), plus
  the cross-runtime regression guard (`foreign`, `list_walk`, `copy_bound` and `gc_bound` stay
  `Runtimes::Both`, each with its own category/contracts: `foreign`/`list_walk` Compute·A.1+A.2,
  `copy_bound` Messaging·A.1+A.5, `gc_bound` Gc·A.1).
- `tests/oracle_smoke.rs` → `baseline` smoke test against the real oracle (no-op skip when unresolved).
- `RESULTS.md` → the definitive results summary (tables, commands, verified skips, host-load record, caveats).
- `results/` → committed raw `Report` JSON + `bench compare` diffs: `m9-*` = the **current** run (with the
  `m9-load-record.md`/`m9-tool-targets-notes.md` records, plus the post-fix tool-resolution smokes
  `m9-bench-plb-smoke.json` / `m9-bench-plb-smoke-path8.json` / `m9-bench-emqx-smoke.json`);
  `m6-*`/`m5-*`/`m4-*` = the previous runs, kept as
  the regression baseline.

## Constraints
- `#![forbid(unsafe_code)]`; **no new crates** (deps resolve offline: `serde`, `serde_json`, `beam_common`).
- External-repo roots are **documented defaults**, overridable via `--repo <id>=<path>` (repeatable)
  and env `BENCH_<ID>_REPO` (e.g. `BENCH_COWBOY_REPO`); no other absolute path is baked in, and the store
  scan never bakes in a hash or version.
- **Never write into the external repos.** Any build/run is staged into a `ScratchDir` copy first (and a
  tool's writable dirs — `HOME`, `MIX_HOME`, `TMPDIR`, … — are redirected into that copy). Never fabricate a metric.
- **Peak memory:** Linux `/proc/<pid>/status` `VmHWM` polling (pure `std::fs`, no `unsafe`) while the
  child runs; if unreadable ⇒ `peak_memory_bytes: null` + a `notes` entry. Erlang-reported
  `erlang:memory(total)` may be recorded as a *separate* named metric, never conflated with OS RSS.
- Files target ~1000 lines; split by responsibility before growing past it.

## Known Issues / Notes for Agents
- **Spawn goes through a bounded `ETXTBSY` retry** (`proc::spawn_with_retry`, mirroring the
  differential harness): both `proc::run` and `proc::spawn` retry ONLY the transient
  `ExecutableFileBusy` / raw-errno-26 fork/exec race (≤5 attempts, ~10 ms backoff) and return every
  other error kind unchanged. It is a bounded retry of a proven scheduling artifact, NOT error
  swallowing — do not remove it. `Runtime::self_check_result` surfaces the errors that outlive it, and
  `Runtime::self_check() -> bool` stays a thin `unwrap_or(false)` wrapper for `info()`/`oracle_smoke`.
- Build/test via the wrapper (read-only `$HOME`): from the repo root
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench`; a real run:
  `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- run --quick --out-file erts_rust/tools/bench/out/target-bench.json`.
  Never hardcode the worktree path.
- Toolchain in this sandbox: `erl`/`erlc`/`escript`/`make`/`mix`/`elixir`/`curl`/`git`/`nix`/`jq`/`clang`
  (21.1.8) **present**, and **`dotnet` IS on `PATH`** (SDK **9.0.318**, `/run/current-system/sw/bin/dotnet`);
  the Nix store also holds a `dotnet-sdk-9.0.318` and two 8.0.425 SDKs, and `command_for_min` picks the
  greatest ≥ 9.0 regardless of source order;
  `rebar3`, `wrk`, `h2load`, `bombardier`, `ab` and `/usr/bin/time` are **absent from `PATH`** — `rebar3`
  resolves from the Nix store via the shared store scan (see above). Network is available (erlang.mk
  can git-fetch cowlib/ranch). Oracle is OTP 29.1.1 / erts-17.1. After
  `scripts/cargo.sh build --release --workspace`, `../../target/release/beam_rt` is built ⇒ the `rust`
  runtime resolves and passes the self-check (`supported: true`).
- **`$BENCH_TOOL_PATH`** (colon-separated `bin` dirs, searched before the store scan) is the documented way
  to force a specific tool build; **`$BENCH_TRY_NIX`** (opt-in, default off) is the last-resort
  `nix shell nixpkgs#<attr> --command <tool>` fallback. Neither is required for the default path.
- **`emqx` needs an OTP-28 toolchain.** It drives EMQX's real `make`/`mix` build, but the checkout pins OTP
  **28.5.0.7-2** (`erlang 28.5.0.7-2` at `.tool-versions:1`, `export OTP_VSN=28.5.0.7-2` at `env.sh:3`) while
  the host `erl` is OTP 29.1.1 / erts-17.1, so `:ezstd`'s `env.mk` make-target (generated via
  `erl -noshell -s init stop -eval …`, an idiom that produces no output under OTP 29 because `init stop` halts
  before the `-eval` — verified on this host) writes nothing and `mix compile` aborts with
  `No rule to make target '…/env.mk'` ⇒ `skipped(emqx-requires-otp-28)`. The gate reads ONLY these declared
  pins (no checkout-layout assumptions) and the note records the pin, its file:line, and the exact host
  `erl` + versions. It is not satisfiable by a version override: the only OTP-28 binding in the store cannot
  be paired with the OTP-29-built Elixir/`mix` the umbrella build needs, and the pin lives in the read-only
  target. Swapping the whole toolchain is out of the driver's scope.
- **`khepri`'s suite may run under an Erlang other than the reported oracle.** The store `rebar3` is an
  escript whose shebang points at an Erlang/OTP 28 `escript`, so it runs the Khepri suite under OTP 28 while a
  `SysOnly` tool-driven row's `runtimes[]` version describes the driver's resolved oracle (`erl` on `PATH`,
  OTP 29). The measured wall time is real; the reported runtime version on such a row describes the driver's
  runtime, not necessarily the Erlang the external tool chose. No metric is fabricated.
- **Cowboy** builds a relx release in the scratch copy (network fetch of cowlib+ranch); the release
  start script `foreground` **`exec`s** `erlexec`, so killing the spawned child stops the VM and its pid
  is the Erlang VM (so `VmHWM` polling works).
- **`plb` requires a ≥ 9.0 `dotnet`; `command_for_min` finds it by version.** PLB's elixir envs target `net9`
  (`bench/tool/BenchTool.csproj`, `bench/include/dotnet/app.csproj`), so `plb` declares `MIN_DOTNET_SDK = &[9]`
  and resolves via `tool::command_for_min`, which selects the greatest `dotnet` ≥ 9.0 across
  `PATH`/`$BENCH_TOOL_PATH`/the store instead of taking `PATH` first. A too-old `PATH` `dotnet` therefore no
  longer shadows a newer store SDK: **smoke-verified** (2026-10-13) both on the default `PATH` and with a real
  8.0.425 SDK forced to the front of `PATH` — `plb` was `ok` both times, the row's notes naming the rejected
  candidates and the resolved 9.0.318. (Before this fix the same condition produced `failed(plb-build-failed)`
  with `NETSDK1045`; that was the m9 measurement run's outcome.) If no `dotnet` is reachable at all ⇒
  `skipped(dotnet-absent)`; if one resolves but none is ≥ 9, the row is a genuine `failed(plb-build-failed)`
  carrying the `requires >= 9` notes — never a fabricated `ok`.
- **The benchmark host is shared and usually loaded.** Absolute latencies/throughputs move several-fold
  with load, because the oracle `erl` (a 14-scheduler SMP VM) loses more to CPU contention than
  `beam_rt`. Prefer the **quiet** primary run and ratios over absolute values, always record
  `/proc/loadavg` with the numbers, and never quote a single contended run as a constant (the recorded
  loads are in `RESULTS.md` §1/§6).
- **Elixir**: the in-repo checkout (`/home/bill/Source/elixir`, `1.21.0-dev`) is unbuilt; the workload
  measures the installed system `elixirc` (1.20.4). Compiling the checkout's stdlib in one `elixirc` batch
  fails on version skew, hence the per-file fallback.
- The external-repo CONTEXT.md files (added read-only, no source changes) record each repo's build/bench
  entry points; consult them via `--repo` overrides.
- **No self-hosted build exists in this crate.** Contract **A.9** is *declared* on `erlc_compiler`
  (`["A.9"]`) and `elixir_compiler` (`["A.1","A.9"]`), but **both run only the system oracle**
  (`Runtimes::SysOnly`): `erlc_compiler` shells out to the system `erlc`, `elixir_compiler` to the installed
  `elixirc`. Nothing in the driver runs any compiler **under `beam_rt`**, so the crate exercises no compiler
  under the Rust runtime and no genuine self-hosted build. Genuine end-to-end `beam_rt` execution is confined
  to `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound` and `gc_bound`; the full target apps (Cowboy
  release, EMQX, Khepri, Elixir) are all `SysOnly` and report `skipped(runtime-unsupported)` under `rust`.
- `out/` is gitignored by `../../.gitignore` (`/tools/bench/out/`); the standard scratch artifact is
  `out/target-bench.json` (via `--out-file`). Durable, reviewable results go to the **tracked** `results/`
  directory (see `RESULTS.md`), which is for committed milestone numbers only.

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `stats` (percentile/mean edge cases), `metrics`/`report`
  (JSON round-trip + schema stability + exactly-one-of value/summary), `cli` (every subcommand, defaults,
  `--help`, `--out-file`, error cases), `probe` decision helpers, `compare` (deltas), `scratch`
  (RAII/copy/skip), `http_load` (loopback keep-alive server + pure parsers), `workloads::tool`
  (tool/argv + the version-aware store ranking + `command_for_min`'s selection: greatest satisfying
  version, tie-break by source order, fallback note, `parse_version_output`, provenance/rejection notes),
  `workloads::plb` (a run with no result JSON is a
  *failed* row, never `ok`), `workloads::foreign`, `workloads::list_walk`, `workloads::copy_bound` and
  `workloads::gc_bound` (case tables, per-side argv, the startup-cancelling per-op/per-call/per-element
  math, deterministic stdout).
- Library integration test: build a synthetic in-memory `Report`, serialize, reload, and compare —
  hermetic, no oracle needed; plus the `Runtimes::Both` regression guard for `foreign`, `list_walk`,
  `copy_bound` and `gc_bound`.
- Oracle-dependent smoke tests (`baseline`; `foreign`/`list_walk`/`copy_bound`/`gc_bound` per-runtime) run
  only when `erl`/`erlc` resolve, and are a no-op skip otherwise, so `cargo test -p bench` stays green in any
  environment.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` GREEN and
  `... test --workspace` GREEN, plus a demonstrated `run --quick` producing real numbers for
  `baseline`/`erlc_compiler`/`elixir_compiler`/`cowboy`/`khepri`/`plb`/`foreign`/`list_walk`/`copy_bound`/
  `gc_bound` and honest `skipped(reason)` rows for the rest. The milestone gate is the `--full` run archived
  in `results/` and summarised in `RESULTS.md`, which must be refreshed whenever the numbers change.

## Runbook — reproducing the sys-vs-rust numbers

Nothing below is required for a code review; it is how the `results/` artifacts and `RESULTS.md` were
produced. Build both the driver and the Rust runtime once, then run the workloads:

```sh
# 1. build the driver AND the runtime (from the repo root; the wrapper supplies a writable CARGO_HOME)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# 2. the FULL cross-runtime report — the reliable source for sys-vs-rust tables:
#    only the six Runtimes::Both workloads, so both sides see the same machine conditions
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m10-bench-xrun.json --verbose

# 2b. the SAME workloads with the interpreter pinned — the Rust-vs-Rust tier-effect control
#     (rust only: it has NO sys rows, so it is never a system-ERTS comparison)
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m10-bench-xrun-interp.json

# 3. the FULL sys-only external targets (erlc/elixirc/cowboy/khepri) in one report
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m10-bench-external.json --verbose

# 4. the tool-driven targets (plb runs; emqx skips on this OTP-29 host)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m10-bench-plb.json  --verbose
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m10-bench-emqx.json --verbose
# (plb resolves the greatest dotnet >= 9.0 by version, so it runs even with an older dotnet on PATH; §4.1 of RESULTS.md)
# (emqx needs an OTP-28 host — here it reports skipped(emqx-requires-otp-28))

# 5. compare (cross-runtime inside one report, or two files)
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m10-bench-xrun.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m10-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m10-bench-xrun.json      # tier effect: interp -> default
```

Archive the `out/` copies into `results/` with only the two mechanical path normalisations
(`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in
`notes` → `$TMPDIR/`). **Never edit a metric value.**

`foreign`, `list_walk`, `copy_bound` and `gc_bound` run under **both** runtimes; every other repo workload is
`sys`-only, so a `--runtime all` report shows a genuine sys-vs-rust row only for `baseline`, `slice`,
`foreign`, `list_walk`, `copy_bound` and `gc_bound`. Read the numbers as *reproducible on this host*, not
milestone results (single host, finite sample counts).

> **Always measure on a quiet host.** The benchmark host is shared and the absolute latencies move
> several-fold with its load: the quiet primary run is the headline and the contended runs are the error
> bar. Check `/proc/loadavg` before and after a run and record it in `RESULTS.md` (which pins the observed
> loads). Run order is runtime-major (all `sys` rows, then all `rust`), so a *drifting* load can bias a
> cross-runtime comparison — the focused `xrun` mitigates this.

**Non-`ok` rows on this host:** only `emqx` → `skipped(emqx-requires-otp-28)` (the checkout pins OTP
**28.5.0.7-2**; host is OTP 29.1.1). `plb` **runs** (a ≥ 9.0 `dotnet` resolves via `command_for_min`,
regardless of `PATH` order); the rest probe ready, and `khepri` runs once the `rebar3` store entry is present.

## Status
- The driver is a complete library + binary: 12 workloads, all implemented and tested; `cargo test -p bench`
  is green (**250 passed / 0 failed** at the m10 commit).
- **Current measurement session: `m10` (2026-10-13 23:06–23:15 UTC, commit `b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`).**
  The full cross-runtime pass ran 12/12 rows `ok` (six `Runtimes::Both` workloads × both runtimes); the
  full sys-only external-target pass ran 4/4 `ok` (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`);
  `plb` → `ok` (version-aware tool resolution picks `dotnet` 9.0.318); the only honest non-`ok` row is
  `emqx` → `skipped(emqx-requires-otp-28)`. The pass also added `m10-bench-xrun-interp.json`, a
  **Rust-only** `BEAM_RT_TIER=interpreter` run that keeps the tier effect separable from the `sys` column.
- Read the figures from `RESULTS.md` / `results/` (`m10-*` current; `m9-*`/`m6-*`/`m5-*`/`m4-*` the
  regression baselines) — this file deliberately restates none of them.

## Measured — where the numbers live

**`RESULTS.md` is the authoritative source for every measured number** (one table per workload category,
the environment record, the exact reproduction commands, the `/proc/loadavg` record, the stability bounds,
the m10-vs-m9 delta section and the ranked bottleneck list). The raw harness JSON behind it is committed
under `results/`: `m10-*` = the **current** run (the full cross-runtime report, the Rust-only interpreter-tier
control, the full external-target report, the PLB/EMQX observations, the cross-runtime/tier/vs-m9 diffs, and
the `m10-load-record.md`/`m10-tool-targets-notes.md` records);
`m9-*`/`m6-*`/`m5-*`/`m4-*` = the previous runs, kept as the regression baseline.
This file deliberately restates none of those numbers — read them there, and regenerate with the Runbook
above rather than quoting a stale copy.

Structural facts that are not numbers (and so belong here):
- `--runtime all` yields a genuine sys-vs-rust row only for the six `Runtimes::Both` workloads
  (`baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound`); every other workload is `SysOnly`
  (see the table above).
- `baseline`'s compute loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1`, so its
  near-parity result reads as the **native floor**, not execution throughput. The per-unit
  execution-throughput signals are `list_walk` (ns/element) and `foreign` (ns/call of real foreign code).
- `slice` runs one call per process, while `foreign`/`list_walk`/`copy_bound`/`gc_bound` loop many ops per
  process at a per-runtime loop count `N`, so peak RSS is like-for-like only for `slice`.

> **`ERTS_STDLIB_DIR` note (a real `beam_rt` behavior):** the Rust runtime resolves its system `ebin` path with
> a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`: `$ERTS_STDLIB_DIR` when set, else the
> standard OTP installation layout under the `erl` on `$PATH` — *resolved to a path, never executed* — then this
> executable's ancestors, `$ERL_TOP`, and standard prefixes); the system `erl` is only *run* as a last resort for
> an installation that layout cannot see. Setting `$ERTS_STDLIB_DIR` still short-circuits the probe.
> `bench` does **not** set it — the harness is a pure driver/observer and never mutates the runtime's environment
> beyond argv — and the per-op/per-call/per-element metric cancels any remaining startup cost by construction.

## Reading the current sys-vs-rust gap (verified)

The `foreign` per-call, `list_walk` per-element, and `copy_bound`/`gc_bound` per-op gaps (their current values
are in `RESULTS.md` §3.4–§3.7) are **real runtime costs, not harness artifacts**. The startup-cancelled metric
reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>` — so CLI
re-entry, per-call module reloads and per-call GC are all ruled out. It is a `beam_rt` per-call host-context
cost: the engine's dispatch arm is itself O(1) and allocation-free per instruction, and the runtime-side costs
behind it are `beam_rt` measurements owned by `../../crates/beam_rt/PERF.md` (the per-call dispatch section)
and `../../crates/beam_rt/CONTEXT.md`, not `bench` output.

**The `beam_rt` execution tier defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit` on
a process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the only
kill switch), and its runtime path compiles **per function, lazily** on a function's first entry — so a fresh
process pays only for the functions it actually enters, and is **not** paying a whole-module compile. The
eager whole-module artifact (`JitBackend::compile`/`CodeCache::get_or_compile`) serves the whole-module
introspection surface and the `beam_engine` `jit_bench` harness only, so `jit_bench` pins the **hot** path and
not the lazy runtime path's per-entry cost. The one-shot cold-start characterisation (a module-graph-heavy
`slice` case at **~1.2–1.3×** the interpreter tier) lives in `../../crates/beam_engine/MEASURED.md` and
`../../crates/beam_rt/PERF.md`.

**The tier effect is now also measured here**, as `results/m10-bench-xrun-interp.json` (the same six workloads,
`--runtime rust` only, with `BEAM_RT_TIER=interpreter` exported — `bench` spawns the runtime with an inherited
environment, so the switch reaches every child). It is a **Rust-vs-Rust** control with **no `sys` rows**: never
read it against the system-ERTS column. `RESULTS.md` §3.9 tabulates it. Two facts to carry forward:
- The baseline JIT is a clear win for `list_walk` (default is **3.84× cheaper** than the interpreter),
  `gc_bound churn` (**1.98×**) and the `foreign` per-call rows (1.20×–1.46×); it is neutral on
  `startup`/`compute`/`etf`.
- The **one inverted row** is the two-process `send_recv` loop: the default tier is **2.15× dearer** than the
  interpreter (79 518 vs 36 969 ns/op) and a focused `--quick` re-run reproduced it at **3.31×**
  (97 518 vs 29 493). `../../crates/beam_rt/PERF.md`'s own snapshot recorded the same direction
  (1.456× worse), so it is a repeatable baseline-JIT message-passing cost — the first item in
  `RESULTS.md` §8 that is a tier *regression* rather than a gap.

Consequence for reading results: startup latency is the trustworthy cross-runtime comparison for
whole-process cost (and, on `slice`, peak RSS); the `foreign`/`list_walk`/`copy_bound`/`gc_bound`
per-call/per-element/per-op rows are snapshots of the term-copy / per-element dispatch / GC cost and should be
re-run via the Runbook rather than adjusted.

## Known Issue — rust peak RSS scales with the loop count `N`

`beam_rt` now has a **GC adoption** (`crates/beam_rt/src/runtime/gc.rs`, `context/gc.rs` — a per-process
collector at the engine's heap-admission point), but it does **not bound peak RSS on the paths `bench`
drives**: a controlled probe (a fresh list built and walked in a tail loop) reads rust peak `VmHWM`
growing with `N` while the oracle stays flat. So the rust `foreign`/`list_walk`/`copy_bound`/`gc_bound`
per-case `case_<name>_peak_rss_bytes` grows with the calibrated loop count and sits **above** the sys
figures — these are **not** like-for-like memory comparisons. `slice` (one call per process, `N`=1) is the
comparable per-process memory metric. The figures are in `RESULTS.md`.

## Honesty audit — measurement-integrity notes (current state)

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always
`failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`,
`src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`.

- **`foreign` classifies a staging failure honestly** (`src/workloads/foreign.rs`: `StageError` +
  `stage_outcome`): a checkout whose foreign `.erl` sources are genuinely absent ⇒
  `skipped(sources-missing)`, while a failure to compile the sources we *did* stage (the embedded driver or a
  present foreign source) ⇒ `failed(module-compile-failed)` — matching the workloads whose sources are ours
  (`list_walk`/`copy_bound`/`gc_bound`). `slice` treats a failed fixture rebuild as `skipped(fixture-missing)`.
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` are computed from the *produced* counts**
  (`rates_per_second` over `CorpusRun::produced` / `produced_loc`, the latter from `compiled_sources`, which
  matches each newly written `.beam` back to its source by file stem), so a partial `erlc` run cannot overstate
  throughput next to `files_compiled`. The attempted file/LOC totals are still reported, but only in a note.
  `elixir_compiler` likewise uses the produced count.
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** — the drain
  threads run only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic — so the
  `unwrap_or_default` masks nothing real; the invariant is documented at the call site.

`baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its
`MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a
fallback classifier.

The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as
`RESULTS.md` states (startup-cancelled `(mean wall@N − mean wall@0) / N`), and the registered-BIF
dispatch cost — **≈ 79–96 ns/call** for the non-inlinable `call_ext` round trip
(`../../crates/beam_rt/PERF.md`, *Registered-BIF (non-inlinable) per-call*) and **≈ 76 ns/iter** for
`pb:tr/2`'s `trunc/1` (`../../crates/beam_rt/TRACE_AND_PERF.md`, *Dispatch cost (registered-BIF path)*) —
is **not** produced here; it is a `beam_rt` measurement.

## See Also
- `RESULTS.md` → the current results summary (one table per workload category, exact reproduction
  commands, verified skip reasons, host-load record, stability, and "what these numbers mean").
- `results/` → the raw harness JSON behind `RESULTS.md` (committed, so the numbers stay reviewable)
  plus the `bench compare` diffs (`m9-*` current, `m6-*`/`m5-*`/`m4-*` previous).
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy, the differential gate, staging.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the concrete target workloads + build/bench entry points.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I → the mandated success metrics (startup, p99, throughput, memory).
