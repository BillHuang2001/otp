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
**Verified in this sandbox:** the store is otherwise subprocess-resolved (`rebar3` 3.27.1, no `PATH` prefix and
no `$BENCH_TRY_NIX`), and `dotnet` resolves to **9.0.318** — on `PATH` (`/run/current-system/sw/bin/dotnet`) or,
when an older `dotnet` is first on `PATH`, from the store `dotnet-sdk-9.0.318` via `command_for_min` — so
`khepri` and `plb` run on this host (§ Known Issues for the exact smoke evidence).
`$BENCH_TRY_NIX` is also verified to work here (it fetches `rebar3` 3.27.1 + Erlang/OTP 28 from `cache.nixos.org`; the network is available), but it stays the last resort described above.
`dotnet` is **store state, not repo state**: PLB needs a ≥ 9.0 SDK, and `command_for_min` finds it wherever it lives (a store entry realized with `nix build --no-link nixpkgs#dotnet-sdk_9`, or a ≥ 9.0 `dotnet` on `PATH`). If no such SDK is reachable, `plb` is either `skipped(dotnet-absent)` or — when an older `dotnet` resolves but none is ≥ 9 — a genuine `failed(plb-build-failed)` with the rejected-candidate notes attached, recorded honestly, never papered over.
**Flake NOTE (do NOT edit `../../flake.nix`):** it lists `rebar3` in `oracleDeps` but no .NET SDK; adding one there would make `plb` reproducible.
The sandbox record of what each tool-driven target actually does here is `results/m14-tool-targets-notes.md`.

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
| `emqx` | messaging | `/home/bill/Source/emqx` (real `make`/`mix` build, in a scratch copy) | **runs** on an OTP-28 host; `skipped(emqx-requires-otp-28)` on this OTP-29 host (checkout pins OTP **28.5.0.7-2** at `env.sh:3`/`.tool-versions:1`; two verified blockers: the `:ezstd` `env.mk` `-s init stop -eval` idiom under OTP ≥ 29 **and** host build tooling/libs absent here — `automake`/`libtool`/`cmake`/`krb5`; full ladder in `results/m12-emqx-enablement-attempt.md`) | `mix-absent`, `make-absent`, `repo-missing`, `not-an-emqx-project`, `emqx-requires-otp-28`, `emqx-build-failed` |
| `slice` | compute | committed `slice.beam` fixture + stdlib wrapper modules, run under **each scheduled runtime** | **runs** (sys+rust) | `erlc-absent`, `fixture-missing`, `runtime-unsupported` |
| `foreign` | compute | **real Cowboy + EMQX modules** (`cowboy_bstr.erl`, `emqx_base62.erl`) compiled with the system `erlc`, run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `sources-missing`, `runtime-unsupported` |
| `list_walk` | compute | in-tree embedded `bench_list_walk` module (BIF-free recursive `sum/1` list walk), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent` |
| `copy_bound` | messaging | in-tree embedded `bench_copy_bound` module (ETF / ETS / two-process copy round-trips, with and without the codec in the receiver), run under **both** runtimes | **runs** (sys+rust) | `erlc-absent`, `module-compile-failed`, `runtime-unsupported` |
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
  `env.sh:3`/`.tool-versions:1` ⇒ `skipped(emqx-requires-otp-28)`). Two verified blockers: the
  `:ezstd` `env.mk` make-target (generated via `erl -noshell -s init stop -eval …`, which OTP ≥ 29
  halts before evaluating) **and**, even with that idiom patched, host build tooling/libs absent here
  (the `:jq` NIF needs `automake`+`libtool`+`cmake`, `:sasl_auth` needs Kerberos headers — `krb5.h`);
  the full enablement ladder is `results/m12-emqx-enablement-attempt.md`, the one-line idiom patch is
  `results/m12-emqx-ezstd-env-mk.patch` (a fetched dep ⇒ apply after `mix deps.get`; **insufficient
  alone**), and the note names both blockers. `mix`/`make` absent ⇒ `mix-absent`/`make-absent`;
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
  `case_<name>_startup_ms`, `case_<name>_peak_rss_bytes`, the throughput metric
  `case_<name>_ns_per_call = (mean(wall@N) − mean(wall@0)) / N × 1e6`, emitted only when positive (else an
  honest note), plus the deterministic, contention-immune companion **`case_<name>_calls = N`** (mirroring
  `copy_bound`'s `case_<name>_ops`) so the loop count behind every marginal is in the report. Every
  runtime's stdout must equal `N × per_call` (recorded in an `observed stdout:` note); a Rust exit `3`
  records the capability gap and emits **no** timing. The Rust row also carries an explicit note that
  `case_<name>_ns_per_call` is **N-dependent** (see Known Issues). `foreign` peak RSS is **not** a
  like-for-like memory comparison (rust RSS scales with the loop count — see Known Issues); `slice` is.
- `list_walk`: **the permanent, reproducible per-element dispatch-throughput signal** — an in-tree embedded
  `bench_list_walk` module with a **BIF-free, non-tail-recursive `sum/1`** over a fixed-length list, built
  by a pure-Erlang `build/2` recursion (no native `lists:seq/2`), so the whole program exercises only the
  engine's per-element dispatch/return path (~7 dispatched instructions per element, the same shape as
  `crates/beam_engine/tests/engine_bench.rs`; the baseline tier does not inline that recursion).
  `walk(N)` sums the list `N` times and returns the deterministic `N × (LEN×(LEN+1)/2)`; `LEN = 1000` (a
  Rust const formatted into the Erlang source so the two cannot drift). The Rust and oracle sides use
  per-runtime loop counts (`n_rust` = 50 000, `n_sys` = 100 000 ⇒ 5×10⁷ vs 10⁸ elements walked; the rust
  `N` is set by the loop-count rule in Constraints). It uses the same startup-cancelled recipe as `foreign`
  (run at `N` and at `0`), reporting `case_list_walk_wall_ms`, `case_list_walk_startup_ms`,
  `case_list_walk_peak_rss_bytes`, `case_list_walk_ns_per_element`
  (`(mean(wall@N) − mean(wall@0)) / (N × LEN) × 1e6`, only when positive) and the **deterministic,
  contention-immune** companion `case_list_walk_elements` = `N × LEN` (stays valid on a loaded host where
  the wall-clock rows do not). Each run's stdout is checked against the deterministic constant; a Rust exit
  `3` records the capability gap and emits **no** timing. `probe()` ⇒ `skipped(erlc-absent)` without `erlc`.
  A `beam_rt` process resolves its system code path with the **native filesystem probe**
  (`crates/beam_rt/src/codeserver.rs`; the `erl` shell-out is only a last resort), not by spawning `erl`.
- `copy_bound`: **the messaging / term-copy-bound metric** — the term-copy boundary run under *both* runtimes. One embedded `bench_copy_bound` module (generated from the crate's constants, so Rust and Erlang cannot drift) with four cases, each copying the **same structured payload** (a 5-key map) and validating the round-trip with a bound-variable pattern (a real equality test that raises on a corrupted copy): `etf_roundtrip` (`term_to_binary/1` + `binary_to_term/1` — the External Term Format wire codec, contract A.5); `ets_copy` (`ets:insert/2` + `ets:lookup/2` — the ETS copy boundary, one table+key per process so the measured work is the copy, not table growth); `send_recv` (a term sent to a second process, round-tripped through `term_to_binary/1` there, and sent back — **both directions** of the process copy boundary, with an `after` guard so a missing reply fails loudly, never hangs); and `send_recv_plain` — the **delivery-only (no-ETF) attribution case**: the identical two-process shape, but the worker replies the payload *directly* (`From ! {self(), T}`) with **no** `term_to_binary`/`binary_to_term` in the receiver, so `send_recv − send_recv_plain` attributes the ETF codec component *inside* the two-process path (the `beam_process`/`beam_message` delivery shape the sibling crates asked for). It uses the shared startup-cancelled per-op recipe (run at `N` and at `0`, with a per-case per-runtime `N`; `etf_roundtrip` 40 000/1 500 000, `ets_copy` 200 000/2 000 000, `send_recv` 10 000/500 000, `send_recv_plain` 15 000/1 000 000 rust/sys — the rust `N` is set by the loop-count rule in Constraints; `--quick` ⇒ 3 runs/case, `--full` ⇒ 10), reporting `case_<name>_wall_ms`, `case_<name>_startup_ms`, `case_<name>_peak_rss_bytes`, `case_<name>_ns_per_op` (`(mean wall@N − mean wall@0) / N × 1e6`, only when positive) and the deterministic, contention-immune `case_<name>_ops` = `N`. A Rust exit `3` records the capability gap and emits **no** timing; an embedded-module compile failure is `failed(module-compile-failed)` (the source is ours), never a skip; `probe()` ⇒ `skipped(erlc-absent)` via the shared `slice::resolve_erlc()` lookup. **Attribution caveat:** `payload/0` is rebuilt inside every iteration, and each iteration also runs the bound-variable equality match, so the `etf_roundtrip` per-op figure is the *whole iteration* (payload construction + `term_to_binary`/`binary_to_term` + the structural equality test), **not the codec alone** — the same shape holds for `ets_copy`/`send_recv`/`send_recv_plain`, and both runtimes pay the identical per-iteration setup.
- `gc_bound`: **the GC-bound metric** — allocate-and-abandon garbage churn under *both* runtimes. One embedded `bench_gc_bound` module whose single `churn/1` case, per iteration, builds and abandons three `LEN`=256-sized structures — a pure-Erlang recursion list (`build_list/2`, *not* a native `lists:seq/2`), a 256-byte binary built from an Erlang-level char list, and a tuple built from another Erlang list — then checks the two O(1)-checkable ones (`byte_size`/`tuple_size`) so a silently-dropped allocation fails loudly. It uses the same shared startup-cancelled per-op recipe (run at `N` and at `0`; `n_rust` = 5 000, `n_sys` = 200 000; `--quick` ⇒ 3 runs, `--full` ⇒ 10), reporting `case_churn_wall_ms`, `case_churn_startup_ms`, `case_churn_peak_rss_bytes`, `case_churn_ns_per_op` (`(mean wall@N − mean wall@0) / N × 1e6`, only when positive) and the deterministic, contention-immune `case_churn_ops` = `N`. The row measures the allocate-and-abandon wall time and **claims no collection count** (whether a runtime's collector runs during the loop is its own admission policy); a Rust exit `3` ⇒ an honest `skipped(unsupported-case)` with **no** timing, and a compile failure is `failed(module-compile-failed)`; `probe()` ⇒ `skipped(erlc-absent)` via `slice::resolve_erlc()`.

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
- `src/marginal.rs` → **the paired, drift-cancelling startup-cancelled marginal**: `Pass` (`units`/`as_str`),
  `PassError`/`Abort`/`PairedRuns`, `sample_pairs(runs, sample)` (Work-then-Startup **within** every round),
  `resolve(&[(work_ms, startup_ms)], units) -> Marginal` (median of the paired deltas, `1.4826 × MAD`
  noise band floored at `MIN_NOISE_MS`, published only above `NOISE_MULTIPLE`), `Marginal::{unit_cost_ns,
  detail, below_resolution_note}`. The sanctioned schedule for every per-unit metric.- `src/report.rs` → `Report`, `WorkloadResult`, `SCHEMA_VERSION`, JSON save/load (serde).
- `src/compare.rs` → diff two `Report`s into per-(workload,runtime,metric) deltas; one-path mode diffs `sys` vs `rust` within a single report.
- `src/run.rs` → runner: resolve runtimes, iterate (runtime × workload), assemble `Report`, write JSON.
- `src/workload.rs` → the `Workload` trait, `RunContext`, `registry()`, `find()`.
- `src/workloads/mod.rs` → submodule wiring; assembles the workload registry; shared repo/file helpers.
- `src/workloads/tool.rs` → shared external-tool resolution (`available`, `command_for` (source order),
  `command_for_min` (version-aware: `Candidate`/`ToolCommand`/`select_candidate`/`probe_tool_version`),
  `extra_tool_dirs`, the Nix-store scan with **version-aware** ranking, `parse_tool_path`, `REPO_SKIP_NAMES`).
- `src/workloads/baseline.rs` · `erlc_compiler.rs` · `elixir_compiler.rs` · `plb.rs` · `cowboy.rs` ·
  `khepri.rs` · `emqx.rs` · `slice.rs` · `foreign.rs` · `list_walk.rs` · `copy_bound.rs` · `gc_bound.rs`
  → one file per workload (probe + run). They carry the loop-count calibration (`const CASE` / `CASES`
  tables + the `the_rust_loop_count(s)_clear_the_boot_jitter_band` tests) and the module docs state the rule.
  `slice.rs` also exposes `pub(crate) resolve_erlc()` (the shared `$ERLC`/`PATH` lookup reused by
  `foreign.rs`, `list_walk.rs`, `copy_bound.rs` and `gc_bound.rs`).
- `tests/integration.rs` → hermetic synthetic-report round-trip + `compare` tests (no oracle needed), plus
  the cross-runtime regression guard (`foreign`, `list_walk`, `copy_bound` and `gc_bound` stay
  `Runtimes::Both`, each with its own category/contracts: `foreign`/`list_walk` Compute·A.1+A.2,
  `copy_bound` Messaging·A.1+A.5, `gc_bound` Gc·A.1).
- `tests/oracle_smoke.rs` → `baseline` smoke test against the real oracle (no-op skip when unresolved).
- `RESULTS.md` → the definitive results summary (per-category tables, both sides named, the load record, the
  reproduction commands, the still-open items, honesty caveats).
- `results/` → committed raw `Report` JSON + supporting artifacts. `m14-*` = the **current** run:
  `m14-recalibration.{md,csv}` (the loop-count recalibration evidence), `m14-bench-xrun.json` (the full
  cross-runtime report), the two `--quick` tier controls `m14-bench-xrun-{interp,quick}.json`, the external
  report `m14-bench-external.json`, the PLB/EMQX rows, the interleaved A/B set
  `m14-ab-{head,m13,m10}-r{1,2,3}.json` + `m14-ab-summary.csv` (rust-vs-rust binary comparison; the
  `$BEAM_RT` variant is selected per run, the foreign sides' programs normalised to
  `$TMPDIR/{m13,m10}wt/erts_rust/target/release/beam_rt`), the `m14-*-load.log` records,
  `m14-load-record.md`, `m14-tool-targets-notes.md`, `m14-tier-effect.csv` and the `m14-compare-*.txt`
  diffs. History: `m13-*`/`m12-*`/`m11-*`/`m10-*`/`m9-*`/`m6-*`/`m5-*`/`m4-*` (regression baselines; the
  `m13-*` group also holds `m13-cli-probe.csv` and `m13-foreign-ladder.csv`, `m9-*` the post-fix
  tool-resolution smokes), the `b54-*`/`b55-*` groups (the Θ(N³) foreign regression experiments + fix), and
  the prose records `m13-load-record.md`/`m13-tool-targets-notes.md`, `m12-foreign-regression.md`,
  `m12-run-route-latency.md`, `m12-emqx-enablement-attempt.md` (+ `.patch`).

## Constraints
- `#![forbid(unsafe_code)]`; **no new crates** (deps resolve offline: `serde`, `serde_json`, `beam_common`).
- **Loop-count calibration (the rust marginal must clear the boot jitter).** Each `Runtimes::Both` loop
  workload declares a **per-runtime loop count `N`** and runs the identical program at `N` and at `0`
  (startup-cancelled). The rust `N` is chosen so the timed loop does enough work that its marginal stands
  clear of the rust side's run-to-run boot jitter — measured ≈±0.1 s, so the rust floor is **≥0.5 s of loop
  work** (≳1.5 s for a case whose marginal has collapsed). `n_sys` stays far above the oracle's tens-of-ms
  jitter. Cross-`N` comparison is honest **only** through the per-op/per-element normalization — never
  compare raw `wall_ms` across two sides with different `N`; every row also emits the deterministic
  `case_*_ops`/`case_*_calls`/`case_*_elements` companion so the `N` behind each figure is in the report.
  Current counts (rust/sys): `list_walk` 50 000/100 000; `copy_bound` `etf_roundtrip` 40 000/1.5 M,
  `ets_copy` 200 000/2 M, `send_recv` 10 000/0.5 M, `send_recv_plain` 15 000/1 M; `gc_bound` `churn`
  5 000/200 000. Each case table's doc comment states the rule + the measured per-unit cost, and in-file
  tests (`…clear_the_boot_jitter_band`) pin the counts.
- External-repo roots are **documented defaults**, overridable via `--repo <id>=<path>` (repeatable)
  and env `BENCH_<ID>_REPO` (e.g. `BENCH_COWBOY_REPO`); no other absolute path is baked in, and the store
  scan never bakes in a hash or version.
- **Never write into the external repos.** Any build/run is staged into a `ScratchDir` copy first (and a
  tool's writable dirs — `HOME`, `MIX_HOME`, `TMPDIR`, … — are redirected into that copy). Never fabricate a metric.
- **Peak memory:** Linux `/proc/<pid>/status` `VmHWM` polling (pure `std::fs`, no `unsafe`) while the
  child runs; if unreadable ⇒ `peak_memory_bytes: null` + a `notes` entry. Erlang-reported
  `erlang:memory(total)` may be recorded as a *separate* named metric, never conflated with OS RSS.
- **Measure one process at a time on this shared host** and record `/proc/loadavg` around every run; read
  ratios and the deterministic counters, not absolute wall times.
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
- **`emqx` needs an OTP-28 toolchain, and that is not its only blocker.** It drives EMQX's real
  `make`/`mix` build, but the checkout pins OTP **28.5.0.7-2** (`erlang 28.5.0.7-2` at `.tool-versions:1`,
  `export OTP_VSN=28.5.0.7-2` at `env.sh:3`) while the host `erl` is OTP 29.1.1 / erts-17.1. Two
  **verified** blockers (full ladder + exact failing output lines in
  `results/m12-emqx-enablement-attempt.md`): (1) `:ezstd`'s `env.mk` make-target is generated via
  `erl -noshell -s init stop -eval …`, an idiom that produces no output under OTP 29 because `init stop`
  halts before the `-eval` — verified on this host — so it writes nothing and `mix compile` aborts with
  `No rule to make target '…/env.mk'`; and (2) **even once that idiom is patched**, the umbrella build
  additionally needs host build tooling/system libraries this sandbox lacks (`:jq`'s `autoreconf`/
  `configure` needs `automake`+`libtool`+`cmake`, and `:sasl_auth` fails on `krb5.h`) — the checkout's own
  CI builds it in a prepared image, not on a bare host. Hence `skipped(emqx-requires-otp-28)`, with a note
  naming the pin, its file:line, the exact host `erl` + versions, and **both** blockers. The gate reads
  ONLY these declared pins (no checkout-layout assumptions) and runs cheaply up front. Not satisfiable by
  a version override: the checkout pin lives in the read-only target and the host tooling gaps are
  independent of it. Swapping in a prepared toolchain/image is out of the driver's scope.
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
  candidates and the resolved 9.0.318. If no `dotnet` is reachable at all ⇒ `skipped(dotnet-absent)`; if one
  resolves but none is ≥ 9, the row is a genuine `failed(plb-build-failed)` carrying the `requires >= 9`
  notes — never a fabricated `ok`.
- **The benchmark host is shared and usually loaded.** Absolute latencies/throughputs move several-fold
  with load, because the oracle `erl` (a 14-scheduler SMP VM) loses more to CPU contention than
  `beam_rt`. Prefer the **quiet** run and ratios over absolute values, always record `/proc/loadavg` with
  the numbers, and never quote a single contended run as a constant (the recorded loads are in `RESULTS.md`
  §1 and `results/m14-*-load.log`).
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
- **`foreign`'s `case_<name>_ns_per_call` is N-dependent on the Rust runtime** (the per-call cost drifts with
  `N`, a common-mode per-op heap retention shared with the m10 baseline), so it is
  only comparable across runs that used the same `N`; each row emits a `case_<name>_calls` companion and an
  explicit note. The rows are **not** a clean cross-runtime ratio, and the loop workloads' rust peak RSS is
  **boot-dominated** (a general per-op heap retention beneath it, not ETS-specific).
  Both facts, with the owner `file:line`s and the root causes (which live **outside this node**), are in the
  dedicated *Known Issue* section below — read it before quoting a `foreign` number or a loop-workload RSS.
- **OPEN (owner `beam_rt`, outside this node) — `foreign: emqx_base62:encode` regression — UNDER
  INVESTIGATION, no cause attributed.** The m13 record reported the m13-era head as **3.2×–3.8× dearer than
  the m10 binary** on `encode`. The m14 interleaved A/B (`results/m14-ab-{head,m13,m10}-r{1,2,3}.json`, load
  88→145, `m14-ab-summary.csv`) **does not reproduce that at head**: within-round `head/m10` = 0.68× / 2.64×
  / 0.76× (median 0.82×, i.e. head ≈ m10), while the **m13 binary does** reproduce it (within-round
  `head/m13` = 0.23× / 0.61× / 0.32×; the m13 binary is 2.4×–4.3× m10). The other three `foreign` cases
  scatter similarly on the loaded host. Another agent is investigating — **no cause is attributed here**
  (`RESULTS.md` §3/§8).

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `stats` (percentile/mean edge cases), `metrics`/`report`
  (JSON round-trip + schema stability + exactly-one-of value/summary), `cli` (every subcommand, defaults,
  `--help`, `--out-file`, error cases), `probe` decision helpers, `compare` (deltas), `scratch`
  (RAII/copy/skip), `http_load` (loopback keep-alive server + pure parsers), `workloads::tool`
  (tool/argv + the version-aware store ranking + `command_for_min`'s selection: greatest satisfying
  version, tie-break by source order, fallback note, `parse_version_output`, provenance/rejection notes),
  `workloads::plb` (a run with no result JSON is a *failed* row, never `ok`), `workloads::foreign`,
  `workloads::list_walk`, `workloads::copy_bound` and `workloads::gc_bound` (case tables, per-side argv,
  the startup-cancelling per-op/per-call/per-element math, deterministic stdout, and the loop-count rule —
  `the_rust_loop_count(s)_clear_the_boot_jitter_band`). The tool-driven targets' **enable/skip decisions** are pinned so a
  future environment change cannot silently turn a real measurement into a skip — `khepri` probes
  `Ready` and its resolved `rebar3` exists on disk when the checkout + tool resolve; `plb` probes
  `Ready` and its version-resolved `dotnet` satisfies `MIN_DOTNET_SDK`; `emqx` gates a pinned checkout
  on a newer host. Each early-returns when its prerequisite is absent, so CI stays green.
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

## Runbook
Nothing here is required for a code review. **`RESULTS.md` §7 is the authoritative, current runbook** (the
exact `bench run …` commands behind every `results/m14-*` artifact); reproduce from there rather than from a
copy. The shape is: build the driver *and* the runtime once
(`erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench` from
the repo root), then drive the subcommands. Two facts to keep in mind when reproducing:
- A `--runtime all` report shows a genuine sys-vs-rust row only for the six `Runtimes::Both` workloads
  (`baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound`); run those as one batch so both
  sides see the same machine conditions.
- **Always measure one process at a time on a quiet host** and record `/proc/loadavg` before and after.
  Run order is runtime-major (all `sys` rows, then all `rust`), so a *drifting* load can bias a cross-runtime
  comparison — use the interleaved A/B for binary-vs-binary claims, and read ratios + the deterministic
  `case_*_ops/calls/elements` counters rather than absolute latencies.
- Archive the `out/` copies into `results/` with only the two mechanical path normalisations
  (`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in
  `notes` → `$TMPDIR/`). **Never edit a metric value.**

## Status
- The driver is a complete library + binary: 12 workloads, all implemented and tested; `cargo test -p bench`
  is green (**259 passed / 0 failed**: 251 lib unit + 4 integration + 1 oracle smoke + 3 doc — the two
  `…clear_the_boot_jitter_band` calibration tests are among the lib units), and the crate builds
  warning-free with `--all-targets`.
- **Current measurement session: `m14` (2026-10-16).** The full cross-runtime pass ran 12/12 rows `ok` (six
  `Runtimes::Both` workloads × both runtimes — **no `marginal below timer resolution` anywhere**), the
  sys-only external pass ran 4/4 `ok` (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`), `plb` → `ok`
  (version-aware resolution picks `dotnet` 9.0.318), and the only honest non-`ok` row in the milestone is
  `emqx` → `skipped(emqx-requires-otp-28)`. Loads: primary pass 56.76→73.66, tier controls 74.70→40.66,
  external 39.89→44.09, `plb`/`emqx` 43.44→59.20, foreign A/B 88.99→145.18.
- **The m13 driver measurement gap is CLOSED.** The rust loop counts were recalibrated so the rust marginal
  clears the boot jitter (Constraint above): `list_walk` 1 000→50 000, `etf_roundtrip` 5 000→40 000,
  `ets_copy` 100 000→200 000, `send_recv` 2 000→10 000, `send_recv_plain` 2 000→15 000, `gc_bound churn`
  2 000→5 000. **43/43 repeated runs resolved** (every rust marginal 5×–150× the ≈±0.1 s jitter band; every
  sys marginal positive) — raw `results/m14-recalibration.csv`, write-up `m14-recalibration.md`.
- Headline m14 numbers (both sides named; read `RESULTS.md` for the tables): startup `p50` **436.5 ms (sys)
  vs 1 373.6 ms (rust)** (p99 788.7 vs 1 835.5), `erlang:memory(total)` **36.2 MB vs 14.6 MB** (rust 2.48×
  lower); primary rust/sys per-unit ratios `list_walk` 5.3×, `etf_roundtrip` 136.3×, `ets_copy` 27.4×,
  `send_recv` 105.4×, `send_recv_plain` 111.9×, `churn` 30.4×, `foreign` 54.5×–79.6×, `slice` ≈2.11×.
- The **`foreign: emqx_base62:encode`** item is still **OPEN / under investigation** (another agent) — the
  m14 A/B does not reproduce the m13-regression at head; see *Known Issues* above.
- Read the figures from `RESULTS.md` / `results/` (`m14-*` current; `m13-*`/`m12-*`/`m11-*`/`m10-*`/`m9-*`/
  `m6-*`/`m5-*`/`m4-*` the regression baselines) — this file deliberately restates none of them.

## Measured — where the numbers live
**`RESULTS.md` is the authoritative source for every measured number** (per-category tables with both sides
named, the environment/load record, the reproduction commands, the calibration rule, the still-open items).
The raw harness behind it is committed under `results/` (`m14-*` current; see the routing-table entry above
for the file list). This file deliberately restates none of the numbers — read them there, and regenerate via
the Runbook rather than quoting a stale copy.

Structural facts that are not numbers (and so belong here):
- `--runtime all` yields a genuine sys-vs-rust row only for the six `Runtimes::Both` workloads
  (`baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound`); every other workload is `SysOnly`.
- `baseline`'s compute loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1`, so a
  near-parity result reads as the **native floor**, not execution throughput. The per-unit
  execution-throughput signals are `list_walk` (ns/element) and `foreign` (ns/call of real foreign code).
- `slice` runs one call per process, while `foreign`/`list_walk`/`copy_bound`/`gc_bound` loop many ops per
  process at a per-runtime loop count `N`, so peak RSS is like-for-like only for `slice`.
- **Rust-vs-Rust rows** (the interpreter-pin tier control and the binary-vs-binary A/B) are labelled
  separately and are **never** a system-ERTS comparison.

> **`ERTS_STDLIB_DIR` note (a real `beam_rt` behavior):** the Rust runtime resolves its system `ebin` path with
> a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`: `$ERTS_STDLIB_DIR` when set, else the
> standard OTP installation layout under the `erl` on `$PATH` — *resolved to a path, never executed* — then this
> executable's ancestors, `$ERL_TOP`, and standard prefixes); the system `erl` is only *run* as a last resort for
> an installation that layout cannot see. Setting `$ERTS_STDLIB_DIR` still short-circuits the probe.
> `bench` does **not** set it — the harness is a pure driver/observer and never mutates the runtime's environment
> beyond argv — and the per-op/per-call/per-element metric cancels any remaining startup cost by construction.

## Reading the current sys-vs-rust gap (verified)
The `foreign` per-call, `list_walk` per-element, and `copy_bound`/`gc_bound` per-op gaps (current values in
`RESULTS.md` §3) are **real runtime costs, not harness artifacts**. The startup-cancelled metric reproduces
with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>` — so CLI re-entry,
per-call module reloads and per-call GC are all ruled out. It is a `beam_rt` per-call host-context cost: the
engine's dispatch arm is itself O(1) and allocation-free per instruction, and the runtime-side costs behind
it are `beam_rt` measurements owned by `../../crates/beam_rt/PERF.md` and `../../crates/beam_rt/CONTEXT.md`,
not `bench` output.

**The `beam_rt` execution tier defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit`
on a process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the
only kill switch), and its runtime path compiles **per function, lazily** on a function's first entry — so a
fresh process pays only for the functions it actually enters, and is **not** paying a whole-module compile.
The eager whole-module artifact (`JitBackend::compile`/`CodeCache::get_or_compile`) serves the whole-module
introspection surface and the `beam_engine` `jit_bench` harness only. The one-shot cold-start
characterisation (a module-graph-heavy `slice` case at **~1.2–1.3×** the interpreter tier) lives in
`../../crates/beam_engine/MEASURED.md` and `../../crates/beam_rt/PERF.md`.

**The tier effect is also measured here** as the two matched-form `--quick` rust-only controls
`results/m14-bench-xrun-interp.json` (interpreter) and `results/m14-bench-xrun-quick.json` (default baseline
JIT) — the same six workloads, `--runtime rust` only, with `BEAM_RT_TIER` exported (`bench` spawns the runtime
with an inherited environment, so the switch reaches every child). They have **no `sys` rows**: they are
Rust-vs-Rust and must never be read against the system-ERTS column. `RESULTS.md` §5 tabulates them off the
matched-form pair (`results/m14-compare-tier-effect-quick.txt`); the as-specified
`results/m14-compare-tier-effect.txt` (interpreter `--quick` vs default `--full`) is a form mismatch. In the
m14 pair the default baseline JIT is cheaper than the interpreter on every case except `send_recv` (a
load-sensitive marginal on a heavily loaded host); every tier table here is a Rust-vs-Rust observation, **NOT**
a system-ERTS comparison.

Consequence for reading results: startup latency is the trustworthy cross-runtime comparison for
whole-process cost (and, on `slice`, peak RSS); the `foreign`/`list_walk`/`copy_bound`/`gc_bound`
per-call/per-element/per-op rows are snapshots of the term-copy / per-element dispatch / GC cost and should
be re-run via the Runbook rather than adjusted.

## Known Issue — the `foreign` per-call metric is `N`-dependent, and the loop workloads' rust peak RSS is boot-dominated

### `foreign` `case_<name>_ns_per_call` is N-dependent on the Rust runtime
On `beam_rt` the per-call cost still drifts with `N` — a **common-mode per-op heap retention shared with the
m10 baseline** — so `foreign`'s `case_<name>_ns_per_call` remains only a clean marginal at the declared `N`
and is **only comparable across runs that used the same `N`**; the `sys` oracle side is ~flat over the same
range. `RESULTS.md` §8 records the verdict; the at-`N` evidence is `results/m14-ab-*` +
`results/m14-ab-summary.csv` (history: the m13 A/B set, the m12 re-derivation
`results/m12-foreign-regression.md`, `results/b54-foreign-growth-curve.md`). This workload's job here is
honesty, not any fix: every row emits the deterministic `case_<name>_calls = N` companion, and the Rust row
carries an explicit N-dependence note, so a per-call figure is never quoted without the `N` behind it.

### the loop workloads' rust peak RSS is boot-dominated
The loop workloads' rust peak RSS is **boot-dominated** — the resident preloaded-module set of the booted
`start.boot` dwarfs the loop's own footprint, so those rows read far above the oracle's and are **not** a
like-for-like memory comparison (`RESULTS.md` §2). Beneath it the same **general per-op heap retention**
remains the underlying mechanism: the BIF-result copy-out (`crates/beam_rt/src/context/calls.rs`) and the
engine's unrooted `alloc_words` seam (`crates/beam_rt/src/context/gc.rs`) both call `Process::grow_nursery`
(`crates/beam_process/src/process.rs`) → `Nursery::grow` (`crates/beam_gc/src/nursery.rs`), which pushes the
replaced whole `HeapSpace` onto `Nursery.retired`; `retired` is cleared only by a real collection
(`Nursery::collapse`), which these loops never trigger. The per-op `.erl` `ByteSize`/`tuple_size` checks still
make a silently-dropped allocation fail loudly — this is a retention, not a correctness, issue.

So the rust `foreign`/`copy_bound` per-case `case_<name>_peak_rss_bytes` is **not** a like-for-like memory
comparison (loop-length- **and** boot-dependent, `RESULTS.md` §2/§8); `slice` (one call per process, `N`=1)
remains the comparable per-process memory metric.

**Reproducing the counter-level evidence (the driver prints none of it).** A `copy_bound`/`gc_bound` row
records only `case_<name>_{wall_ms,startup_ms,peak_rss_bytes,ns_per_op,ops}` plus notes — it prints **no**
`collections`/`minor_gcs`/`heap_size`. Those per-process counters come from a **probe module** of your own
(`erlang:process_info(self(), heap_size | total_heap_size | garbage_collection)` +
`erlang:statistics(garbage_collection)`) that calls the staged `bench_copy_bound:<case>/1` and is run through
the **`run` action** (`beam_rt -pa <dir> run <probe.beam> <probe>:<f>/1 <N>`) — the same route the `rust`
side of every `Runtimes::Both` loop workload uses, which is what exposes the loop's real scheduled-process
nursery. The `-eval` route does **not**: there the counters stay at their fresh values (the growth is
invisible through `process_info(self(), _)`) even though the run's peak RSS still grows, so counter
measurements must go through `run`. (`heap_size` is `Nursery::capacity_words` — the **sum over retired +
current** spaces — so it is the retained-capacity signal.) The measured values and root causes are the
`beam_rt` node's (`../crates/beam_rt/src/context/CONTEXT.md`, `M1_GAPS.md`, `PERF.md`), not this node's.

## Honesty audit — measurement-integrity notes (current state)
No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always
`failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`,
`src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`, no `#[ignore]`; the only `unreachable!`s
(three in `src/compare.rs`) are invariant assertions that carry an explanatory message (an index-union /
impossibility guard), not silent error handling.

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
- `baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its
  `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a
  fallback classifier.

The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as
`RESULTS.md` states (startup-cancelled `(mean wall@N − mean wall@0) / N`, with a per-runtime `N` calibrated per
the loop-count rule), and the registered-BIF dispatch cost is **not** produced here; it is a `beam_rt`
measurement (`../../crates/beam_rt/PERF.md`, `TRACE_AND_PERF.md`).

## See Also
- `RESULTS.md` → the current (m14) results summary (per-category tables, both sides named, exact reproduction
  commands, verified skip reasons, the load record, the calibration rule, the still-open items).
- `results/` → the raw harness behind `RESULTS.md` (committed, so the numbers stay reviewable); see the
  routing-table entry above for the `m14-*` set and the history groups.
- `results/m12-emqx-ezstd-env-mk.patch` → the literal, reproducible `:ezstd` `env.mk` idiom patch (a
  *fetched* dep, applied after `mix deps.get`; **insufficient alone** — the build then needs missing host
  `automake`/`libtool`/`cmake`/`krb5`), which is why the driver applies no patch and keeps its honest skip.
- `results/m12-emqx-enablement-attempt.md` → the verified, bounded `emqx` enablement attempt (exact
  commands, each blocker's exact failing output) establishing that EMQX-enterprise is unbuildable here:
  the `:ezstd` OTP-29 idiom **plus** missing host build tooling/libs ⇒ the driver's `emqx` skip is honest.
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy, the differential gate, staging.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the concrete target workloads + build/bench entry points.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I → the mandated success metrics (startup, p99, throughput, memory).
