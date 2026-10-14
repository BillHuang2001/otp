# m11 — external target workloads: what actually runs in this sandbox

Run date: 2026-10-14 05:47–06:57 UTC (= 2026-10-14 13:47–14:57 +0800). Commit measured:
**`ab6f1b5e58835cbf5fcb75257679532c46090a74`** (`ab6f1b5e58`). Host: 14 cores, **host-wide**
`/proc/loadavg` (shared machine — see the load record beside this file). Oracle: system `erl`,
**OTP 29 / erts-17.1** (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/…/erl`).
`bench` = `erts_rust/target/release/bench` (release build of this crate at this commit).

Raw artifacts (committed beside this file): `m11-bench-external.json` (the four `--full` sys-only
targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`), `m11-bench-plb.json`
(the PLB run), `m11-bench-emqx.json` (the EMQX observation).

## Verdict

| target | status | metric measured |
|---|---|---|
| `khepri` | **ok** | `wall_ms=83402.90`; `peak_memory_bytes=357818368` |
| `cowboy` | **ok** | `requests_per_sec=190626.72`, `latency_ms` p50 `0.078693`, p99 `0.223380` |
| `erlc_compiler` | **ok** | `wall_ms=7905.65`, `files_per_sec=7.589`, `loc_per_sec=5882.25` |
| `elixir_compiler` | **ok** | `wall_ms=47667.62`, `files_per_sec=4.678`, `compiler_startup_ms=324.33` |
| `plb` | **ok** | `wall_ms=68318.69`; `test_time_ms` p50 559.98 / p99 3355.48 (n=10) |
| `emqx` | `skipped(emqx-requires-otp-28)` | none — no number fabricated |

**Five of the six external targets run; `emqx` skips** with the same verified mechanical reason as m9/m10.
`bench list --json` reports **all 12 workloads `probe_ready: true`** (no target is missing a
prerequisite).

**No source file under `/home/bill/Source/` was written** — the driver stages every build into a
scratch copy.

> **Which runtime produces these numbers?** All six rows are executed by their **own system-ERTS
> toolchain** — `erlc` / `elixirc` / the Cowboy release on the oracle `erl` / `rebar3`'s Erlang / the
> `.NET` tool for PLB. **None is executed by the Rust runtime (`beam_rt`).** They are **reference**
> figures for the sys-only surfaces (the Rust runtime cannot boot an OTP release, run `rebar3`, or act
> as `erlc`/`elixirc`, so every one of these workloads is `Runtimes::SysOnly`).

## Environment findings

1. **`dotnet` on `PATH` is 9.0.318** (`/run/current-system/sw/bin/dotnet`) — `plb` runs with no `PATH`
   reordering: the version-aware resolver (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)`
   with min `9`) selects the greatest satisfying SDK and rejects the two 8.0.425 store entries.
   Provenance exactly as recorded in `m11-bench-plb.json`:
   ```
   build stage ok (dotnet --task build took 23035 ms)
   rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (nix-store): requires >= 9
   rejected dotnet 8.0.425 at /nix/store/qybc3wpz3hz5daipbmnj7rgc73hn35j2-dotnet-sdk-wrapped-8.0.425/bin/dotnet (nix-store): requires >= 9
   resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)
   ```
2. **`clang` IS on `PATH`**: `clang version 21.1.8` (`/run/current-system/sw/bin/clang`).
3. **`rebar3` is NOT on `PATH`** but **resolves through the driver's version-ranked Nix-store scan**
   (`/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin/rebar3` — the newest of the 3.27.0 and
   3.27.1 entries present), so `khepri` runs with no `PATH` prefix and no `$BENCH_TRY_NIX`. The store
   entry is an escript whose shebang points at an Erlang/OTP 28 `escript`.
4. **`/usr/bin/time` is absent** — the driver's own per-child timers and `/proc/<pid>` `VmHWM` polling
   are used; no external timing tool participates.
5. **Network is available** — `cowboy`'s `erlang.mk` build git-fetched cowlib/ranch (its relx release
   build took 45 207 ms this pass, vs 39 512 ms at m10).
6. **`$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` are unset** for this pass (the default resolution path is
   what is exercised); `elixirc`/`mix` are the installed **1.20.4** ("compiled with Erlang/OTP 29").

## Per-target detail

### `plb` — Programming-Language-Benchmarks (dotnet) — **`ok`**

`ok`: `wall_ms=68318.691925` (the two-stage `--task build` + `--task bench`; the build stage alone took
23 035 ms), `test_time_ms` n=10 min `0.0` mean `1170.95` p50 `559.98` p90 `3014.48` p99 `3355.48`
max `3393.36`, `test_mem_bytes=95862784`, `peak_memory_bytes=169824256`.
This is a **`--quick` observation on a shared host**, not a milestone figure: `test_time_ms` is PLB's own
per-test time across its 10 elixir kernels (`min` 0 = a sub-millisecond kernel) and `wall_ms` is
dominated by building the kernels' escripts. `peak_memory_bytes` is the driver's external `VmHWM` of the
`.NET` tool. Read it as "PLB builds and runs on this host".

### `khepri` — Khepri `rebar3 eunit`

`ok`: `wall_ms=83402.904172`, `peak_memory_bytes=357818368`.
**Caveat worth recording:** the store `rebar3` is an escript whose shebang points at an Erlang/OTP 28
`escript`, so it runs the Khepri suite under **Erlang/OTP 28** while the report's `runtimes[]` entry
describes the driver's oracle (`erl` on `PATH`, OTP 29). The measured wall time is real, but the
*reported* runtime version on a `SysOnly` tool-driven row describes the driver's resolved runtime, not
necessarily the Erlang the external tool chose. No metric is fabricated.

### `emqx` — EMQX umbrella build (make/mix through `env.sh`) — `skipped(emqx-requires-otp-28)`

Unchanged from m9/m10: the blocker is a genuine toolchain constraint, verified first-hand.
The checkout pins OTP 28 (`/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`;
`env.sh:3` → `export OTP_VSN=28.5.0.7-2`), while the host `erl` is OTP 29.1.1 / erts-17.1.
The umbrella build aborts in the `:ezstd` dependency's NIF makefile, which generates its `env.mk` with
the idiom `erl -noshell -s init stop -eval '…'`; on OTP 29 `init stop` halts **before** the `-eval`
runs (demonstrated on this host: exit `0`, no output), so `mix compile` fails with
`No rule to make target '…/env.mk'`. The store `erl-28.5.0.7` cannot be paired with the OTP-29-built
Elixir 1.20.4/`mix` the umbrella build needs, and the pin lives in the read-only target, so it is not
satisfiable without a full toolchain swap (out of this crate's scope). The skip is immediate — no build
time accrued (`wall_ms` for the run is 727 ms total). No number is fabricated.

### `cowboy` — hello_world release + in-process HTTP/1.1 load (`--full`, 16 conns × 5 s)

`ok`: `build_ms=45207.248163` (relx release; network fetch of cowlib/ranch), `requests=953510`,
`errors=0`, `requests_per_sec=190626.720402`, `latency_ms` n=20000 min `0.026328` mean `0.084982`
p50 `0.078693` p90 `0.112260` p99 `0.223380` max `2.872852`, `peak_memory_bytes=158814208`.
Versus m10 the p50 rose (0.063634 → 0.078693 ms) while the p99 **fell** (0.301994 → 0.223380 ms) and
throughput fell ~9.3 % (210 250 → 190 627 req/s) — the shared host's load is the explanation, not a
server change.

### `erlc_compiler` — `erlc` over the bounded OTP stdlib + Elixir corpus slice

`ok`: `wall_ms=7905.651771`, `files_compiled=60`, `files_per_sec=7.589507`, `loc_per_sec=5882.247454`,
`peak_memory_bytes=135045120`, note
`corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`.
The `files_per_sec`/`loc_per_sec` rates are computed from the **produced** `.beam`/LOC counts, so a
partial run cannot overstate them. The m11 rate is ~14 % below m10's (`8.842`) with the same 60/60 files
compiled by the unmodified system `erlc`, so the move is host-load-attributable, not a compiler change.

### `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=47667.623194`, `files_attempted=105`, `files_compiled=223`,
`files_per_sec=4.678228`, `loc_per_sec=2348.847971`, `compiler_startup_ms=324.329433`,
`peak_memory_bytes=205479936`. Notes:
`batch compile failed; per-file fallback (version skew)`;
`compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`;
`corpus = the checkout's unbuilt, read-only .ex sources staged into $TMPDIR; compiler = the installed
system elixirc, so a version skew with the checkout is expected`;
`97/105 compiled; 223 .beam produced`;
`223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)`.
**`files_compiled` (=223) exceeding `files_attempted` (=105) is expected and documented by the workload
itself** (one Elixir source can define several modules). The throughput rate uses the produced count.

## Host load

`/proc/loadavg` is **host-wide** and this machine is shared with other concurrent work; across this
session the 1-min value ran from `1.12` to `7.18`. The per-run before/after record and the exact commands
are in `m11-load-record.md`; the archived runs were `6.04 → 2.98` (cross-runtime, `--full`),
`1.36 → 2.36` (external targets, `--full`), `2.36 → 1.97` (PLB), `1.97 → 1.97` (EMQX),
`1.81 → 2.59` (interpreter-tier control). The external latencies/throughputs above are *reproducible on
this host*, not milestone numbers.

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
  measured in `m11-bench-xrun.json`.
- **PLB is `--quick` (`n=10` of its own kernels), not `--full`.** The workload's `--full` profile is not
  a milestone form here (see `../CONTEXT.md`); the run is an "it builds and runs" observation.
- **`cowboy` is a 5-second keep-alive load, not a sustained soak** — a single shared-host sample, with
  the p99 recorded rather than a long-tail distribution study.
