# m12 — external target workloads: what actually runs in this sandbox

Run date: 2026-10-15 14:00–15:03 UTC (= 2026-10-15 22:00–23:03 +0800). Commit measured:
**`ee3e426db0`** (runtime+driver tree; the m12 measurement artifacts were committed as `bdadd02333`).
Host: 14 cores, **host-wide** `/proc/loadavg` (shared machine — see the load record beside this file).
Oracle: system `erl`, **OTP 29.1.1 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`).
`bench` = `erts_rust/target/release/bench` (release build of this crate at this commit).

Raw artifacts (committed beside this file): `m12-bench-external.json` (the four `--full` sys-only
targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`), `m12-bench-plb.json`
(the PLB run), `m12-bench-emqx.json` (the EMQX observation). Diff vs m11:
`m12-compare-vs-m11-external.txt`.

## Verdict

| target | status | metric measured |
|---|---|---|
| `khepri` | **ok** | `wall_ms=77564.34`; `peak_memory_bytes=374722560` |
| `cowboy` | **ok** | `requests_per_sec=165489.21`, `latency_ms` p50 `0.077852`, p99 `0.742380` |
| `erlc_compiler` | **ok** | `wall_ms=6842.18`, `files_per_sec=8.769`, `loc_per_sec=6796.52` |
| `elixir_compiler` | **ok** | `wall_ms=50304.34`, `files_per_sec=4.433`, `compiler_startup_ms=297.64` |
| `plb` | **ok** | `wall_ms=70648.46`; `test_time_ms` p50 595.13 / p99 3565.99 (n=10) |
| `emqx` | `skipped(emqx-requires-otp-28)` | none — no number fabricated |

**Five of the six external targets run; `emqx` skips** with the same verified mechanical reason as
m9/m10/m11. `bench list --json` reports **all 12 workloads `probe_ready: true`** (no target is missing a
prerequisite), the same as m11.

**No source file under `/home/bill/Source/` was written** — the driver stages every build into a
scratch copy (verified: the read-only checkouts stay clean).

> **Which runtime produces these numbers?** All six rows are executed by their **own system-ERTS
> toolchain** — `erlc` / `elixirc` / the Cowboy release on the oracle `erl` / `rebar3`'s Erlang / the
> `.NET` tool for PLB. **None is executed by the Rust runtime (`beam_rt`).** They are **reference**
> figures for the sys-only surfaces (the Rust runtime cannot boot an OTP release, run `rebar3`, or act
> as `erlc`/`elixirc`, so every one of these workloads is `Runtimes::SysOnly`).

> **Host-load caveat.** Every wall/latency/throughput figure below is on a shared, usually
> oversubscribed 14-core host: the external-target pass was bracketed by 1-min `/proc/loadavg` `10.91`
> → `9.34`, the PLB pass by `9.34` → `7.85`, the EMQX pass by `7.85` → `7.54`. Read the numbers as
> *reproducible on this host*, never as machine-speed constants; the m11→m12 deltas are within the
> load band.

## Environment findings

1. **`dotnet` on `PATH` is 9.0.318** (`/run/current-system/sw/bin/dotnet`) — `plb` runs with no `PATH`
   reordering: the version-aware resolver (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)`
   with min `9`) selects the greatest satisfying SDK and rejects the two 8.0.425 store entries.
   Provenance exactly as recorded in `m12-bench-plb.json`:
   ```
   build stage ok (dotnet --task build took 23330 ms)
   rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (nix-store): requires >= 9
   rejected dotnet 8.0.425 at /nix/store/qybc3wpz3hz5daipbmnj7rgc73hn35j2-dotnet-sdk-wrapped-8.0.425/bin/dotnet (nix-store): requires >= 9
   resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)
   ```
2. **`clang` IS on `PATH`**: `clang version 21.1.8` (`/run/current-system/sw/bin/clang`).
3. **`rebar3` is NOT on `PATH`** but **resolves through the driver's version-ranked Nix-store scan**
   (`rebar3-3.27.1`), so `khepri` runs with no `PATH` prefix and no `$BENCH_TRY_NIX`. The store entry is
   an escript whose shebang points at an Erlang/OTP 28 `escript`.
4. **`/usr/bin/time` is absent** — the driver's own per-child timers and `/proc/<pid>` `VmHWM` polling
   are used; no external timing tool participates.
5. **Network is available** — `cowboy`'s `erlang.mk` build git-fetched cowlib/ranch (its relx release
   build took 42 527 ms this pass, vs 45 207 ms at m11 and 39 512 ms at m10 — host-load variation).
6. **`$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` are unset** for this pass (the default resolution path is
   what is exercised); `elixirc`/`mix` are the installed **1.20.4** ("compiled with Erlang/OTP 29").

## Per-target detail

### `plb` — Programming-Language-Benchmarks (dotnet) — **`ok`**

`ok`: `wall_ms=70648.460241` (the two-stage `--task build` + `--task bench`; the build stage alone took
23 330 ms), `test_time_ms` n=10 min `0.0` mean `1245.27` p50 `595.13` p90 `3408.24` p99 `3565.99`
max `3583.52`, `test_mem_bytes=94777344`, `peak_memory_bytes=171573248`.
This is a **`--quick` observation on a shared host**, not a milestone figure: `test_time_ms` is PLB's own
per-test time across its 10 elixir kernels (`min` 0 = a sub-millisecond kernel) and `wall_ms` is
dominated by building the kernels' escripts. `peak_memory_bytes` is the driver's external `VmHWM` of the
`.NET` tool. Read it as "PLB builds and runs on this host". Versus m11: wall 68 318.69 → 70 648.46 ms
(**+3.41 %**, read manually from the two `m12-bench-plb.json`/`m11-bench-plb.json` rows — no tool diff
archived for `plb`), i.e. flat within the host's load band.

### `khepri` — Khepri `rebar3 eunit` — **`ok`**

`ok`: `wall_ms=77564.335456`, `peak_memory_bytes=374722560` (m11: 83 402.90 ms / 357 818 368 B, so
**−7.00 %** wall). **Caveat worth recording:** the store `rebar3` is an escript whose shebang points at
an Erlang/OTP 28 `escript`, so it runs the Khepri suite under **Erlang/OTP 28** while the report's
`runtimes[]` entry describes the driver's oracle (`erl` on `PATH`, OTP 29). The measured wall time is
real, but the *reported* runtime version on a `SysOnly` tool-driven row describes the driver's resolved
runtime, not necessarily the Erlang the external tool chose. No metric is fabricated.

### `emqx` — EMQX umbrella build (make/mix through `env.sh`) — `skipped(emqx-requires-otp-28)`

Unchanged from m9/m10/m11: the blocker is a genuine toolchain constraint, verified first-hand. The
checkout pins OTP 28 (`/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`; `env.sh:3` →
`export OTP_VSN=28.5.0.7-2`), while the host `erl` is OTP 29.1.1 / erts-17.1. The umbrella build aborts
in the `:ezstd` dependency's NIF makefile, which generates its `env.mk` with the idiom
`erl -noshell -s init stop -eval '…'`; on OTP 29 `init stop` halts **before** the `-eval` runs
(demonstrated on this host: exit `0`, no output), so `mix compile` fails with
`No rule to make target '…/env.mk'`. The store `erl-28.5.0.7` cannot be paired with the OTP-29-built
Elixir 1.20.4/`mix` the umbrella build needs, and the pin lives in the read-only target, so it is not
satisfiable without a full toolchain swap (out of this crate's scope). The skip is immediate — no build
time accrued (the run took 1 s). No number is fabricated.

### `cowboy` — hello_world release + in-process HTTP/1.1 load (`--full`, 16 conns × 5 s)

`ok`: `build_ms=42527.108981` (relx release; network fetch of cowlib/ranch), `requests=827888`,
`errors=0`, `requests_per_sec=165489.213469`, `latency_ms` n=20000 min `0.023990` mean `0.105668`
p50 `0.077852` p90 `0.137818` p99 `0.742380` max `3.268676`, `peak_memory_bytes=166875136`.
Versus m11 the throughput **fell** 190 626.72 → 165 489.21 req/s (**−13.19 %**) and the mean latency rose
(+24.34 %) while the p99 **rose** (0.223380 → 0.742380 ms) — the shared host's load is the explanation
(the pass was bracketed by loadavg 10.91 → 9.34), not a server change.

### `erlc_compiler` — `erlc` over the bounded OTP stdlib + Elixir corpus slice

`ok`: `wall_ms=6842.177446`, `files_compiled=60`, `files_per_sec=8.769138`,
`loc_per_sec=6796.520606`, `peak_memory_bytes=134492160`, note
`corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`.
The `files_per_sec`/`loc_per_sec` rates are computed from the **produced** `.beam`/LOC counts, so a
partial run cannot overstate them. The m12 rate is ~15.5 % **above** m11's (`7.589`) with the same 60/60
files compiled by the unmodified system `erlc`, and the wall fell 7 905.65 → 6 842.18 ms (**−13.45 %**) —
host-load variation, not a compiler change.

### `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=50304.338719`, `files_attempted=105`, `files_compiled=223`, `files_per_sec=4.433017`,
`loc_per_sec=2225.732469`, `compiler_startup_ms=297.643093`, `peak_memory_bytes=211509248`. Notes:
`batch compile failed; per-file fallback (version skew)`;
`compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`;
`corpus = the checkout's unbuilt, read-only .ex sources staged into $TMPDIR; compiler = the installed
system elixirc, so a version skew with the checkout is expected`;
`97/105 compiled; 223 .beam produced`;
`223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)`.
**`files_compiled` (=223) exceeding `files_attempted` (=105) is expected and documented by the workload
itself** (one Elixir source can define several modules). The throughput rate uses the produced count.
Versus m11: wall 47 667.62 → 50 304.34 ms (+5.53 %), files/s −5.24 %, compiler startup −8.23 %.

## Host load

`/proc/loadavg` is **host-wide** and this machine is shared with other concurrent work; across this
session the 1-min value ran from `3.91` to `65.28`. The per-run before/after record and the exact
commands are in `m12-load-record.md`; the archived runs were `65.28 → 6.93` (cross-runtime, `--full`),
`3.91 → 11.69` (the two tier controls), `10.91 → 9.34` (external targets, `--full`), `9.34 → 7.85`
(PLB), `7.85 → 7.54` (EMQX), `10.26 → 8.86` (the run-route probes), `6.72 → 5.73` (the `foreign` A/B).
The external latencies/throughputs above are *reproducible on this host*, not milestone numbers.

## Reproducibility note

The `.NET` SDK situation is *tool/environment* state, not repository state: the version-aware resolver
(`tool::command_for_min`) picks the greatest `dotnet` ≥ 9.0 across `PATH` + `$BENCH_TOOL_PATH` + the Nix
store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH`.
`khepri` needs only the `rebar3` store entry and runs as-is. `emqx` remains a genuine toolchain skip on
any host without an OTP-28 Erlang that can pair with the Elixir/`mix` the umbrella build uses.

## Not measured in this pass (and why)

- **No Rust-runtime row for any of these targets.** All six are `Runtimes::SysOnly` by construction (a
  `rust` schedule would report `skipped(runtime-unsupported)`, never a number); the six workloads that
  *do* run under `beam_rt` are `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound` and `gc_bound`,
  measured in `m12-bench-xrun.json`.
- **PLB is `--quick` (`n=10` of its own kernels), not `--full`.** The workload's `--full` profile is not
  a milestone form here (see `../CONTEXT.md`); the run is an "it builds and runs" observation.
- **`cowboy` is a 5-second keep-alive load, not a sustained soak** — a single shared-host sample, with
  the p99 recorded rather than a long-tail distribution study.
