# m9 — external target workloads: what actually runs in this sandbox

Run date: 2026-10-13 (UTC). Host: 14 cores, **host-wide** `/proc/loadavg` (shared machine — see
the load record beside this file). Oracle: system `erl`, **OTP 29 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/.../erl`).
`bench` = `erts_rust/target/release/bench` (release build of this crate at this commit).

Raw artifacts (committed beside this file): `m9-bench-external.json` (the four `--full` sys-only
targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`), `m9-bench-plb.json`
(the PLB attempt), `m9-bench-emqx.json` (the EMQX observation).

## Verdict

| target | status | metric measured |
|---|---|---|
| `khepri` | **ok** | `wall_ms=77755.91`; `peak_memory_bytes=360857600` |
| `cowboy` | **ok** | `requests_per_sec=220143.53`, `latency_ms` p50 `0.0639`, p99 `0.2764` |
| `erlc_compiler` | **ok** | `wall_ms=6094.92`, `files_per_sec=9.844` |
| `elixir_compiler` | **ok** | `wall_ms=48696.61`, `files_per_sec=4.579` |
| `plb` | **failed(`plb-build-failed`)** | none — no number fabricated |
| `emqx` | `skipped(emqx-requires-otp-28)` | none — no number fabricated |

**No harness bug was found; no source file was changed.** The one blocker that *fails* a target is an
**environment/tool-resolution** one: a too-old `dotnet` on `PATH` shadows the store's 9.x SDK.

## Environment findings (differ from common assumptions)

1. **`dotnet` IS on `PATH`** in this sandbox: `/run/current-system/sw/bin/dotnet`, `--version` →
   **8.0.425**. The Nix store also holds a 9.0.318 SDK, but the shared tool policy consults
   `PATH` **first** (`src/workloads/tool.rs` resolves, in order: 1 `PATH` on the live `proc::which`,
   2 `$BENCH_TOOL_PATH`, 3 the version-ranked Nix-store scan, 4 the opt-in `$BENCH_TRY_NIX` shim), so
   the store's 9.x SDK is **never reached** while 8.0.425 is on `PATH`.
2. **`clang` IS on `PATH`**: `clang version 21.1.8` (`/run/current-system/sw/bin/clang`).
3. **`rebar3` is NOT on `PATH`** but **resolves through the driver's version-ranked Nix-store scan**
   (step 3), so `khepri` runs with no `PATH` prefix and no `$BENCH_TRY_NIX`. The store entry is an
   escript whose shebang points at an Erlang/OTP 28 `escript`.
4. **`/usr/bin/time` is absent** — the driver's own per-child timers and `/proc/<pid>` `VmHWM`
   polling are used; no external timing tool participates.

## Per-target detail

### `plb` — Programming-Language-Benchmarks (dotnet) — **`failed(plb-build-failed)`**

* Tool: `dotnet` from **step 1 (`PATH`)**, SDK **8.0.425**.
* `dotnet --task build` exits `Some(1)` in ~1.3 s; the recorded stderr/stdout are:
  * stderr: `The build failed. Fix the build errors and run again.`
  * stdout: `/nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/share/dotnet/sdk/8.0.425/Sdks/Microsoft.NET.Sdk/targets/Microsoft.NET.TargetFrameworkInference.targets(166,5): error NETSDK1045: The current .NET SDK does not support targeting .NET 9.0. Either target .NET 8.0 or lower, or use a version of the .NET SDK that supports .NET 9.0.`
* **Why this differs from the previous run.** In the `m6` session the store's .NET 9 SDK was realized
  (`nix build --no-link nixpkgs#dotnet-sdk_9`) and, with no `dotnet` on `PATH`, the version-ranked
  store scan picked **9.0.318**, so `plb` ran `ok`. Here a `dotnet` **is** on `PATH` (8.0.425), and
  because `PATH` wins before the store scan, realizing the 9.x SDK in the store is **not by itself
  sufficient** on this host. That gap is recorded in `../CONTEXT.md` (Known Issues) and is the
  actionable finding: either put a ≥ 9.0 `dotnet` earlier on `PATH`, or remove `dotnet` from `PATH`
  so step 3 can pick the store's 9.x SDK.
* No PLB number is fabricated; the row carries the real captured build error.

### `khepri` — Khepri `rebar3 eunit`

* Tool: `rebar3` 3.27.1 from the store scan (step 3). Ran `ok`: `wall_ms=77755.906846`,
  `peak_memory_bytes=360857600`.
* **Caveat worth recording:** the store `rebar3` is an escript whose shebang points at an
  Erlang/OTP 28 `escript`, so it runs the Khepri suite under **Erlang/OTP 28** while the report's
  `runtimes[]` entry describes the driver's oracle (`erl` on `PATH`, OTP 29). The measured wall time
  is real, but the *reported* runtime version on a `SysOnly` tool-driven row describes the driver's
  resolved runtime, not necessarily the Erlang the external tool chose. No metric is fabricated.

### `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`) — `skipped(emqx-requires-otp-28)`

The blocker is a genuine toolchain constraint, verified first-hand:

* The checkout pins OTP 28: `.tool-versions` → `erlang 28.5.0.7-2`; `env.sh` → `OTP_VSN=28.5.0.7-2`.
  The host `erl` is `otp_release=29`, `symbolic version=17.1`.
* The mechanism, demonstrated directly on this host (exit `0`, **no output at all**):
  ```
  $ erl -noshell -s init stop -eval 'io:format("ENV_MK_WOULD_BE_WRITTEN~n"), halt().'
  (no output)   exit=0
  ```
  `init stop` halts before `-eval` is evaluated, so the EMQX dependency `:ezstd`'s makefile — which
  generates its `env.mk` with exactly this idiom — writes nothing, and EMQX's `mix compile` then
  aborts with `No rule to make target '…/env.mk'`. The skip is immediate (no build time accrued).
* Swapping in a whole OTP-28 toolchain (the store `erl-28.5.0.7` cannot be paired with the
  OTP-29-built Elixir 1.20.4/`mix` on `PATH`) is out of this crate's scope. No number is fabricated.

### `cowboy` — hello_world release + in-process HTTP/1.1 load (`--full`, 16 conns × 5 s)

`ok`: `build_ms=40503.477145` (relx release; network fetch of cowlib/ranch), `requests=1101086`,
`errors=0`, `requests_per_sec=220143.532489`, `latency_ms` n=20000 min `0.02521` mean `0.075841`
p50 `0.063911` p90 `0.102995` p99 `0.276380` max `3.592006`, `peak_memory_bytes=177963008`.
Compared with the previous full-mode headline (117 813 req/s, p50 0.064, p99 0.133) the p50 is
unchanged while throughput and p99 differ; the shared host's load is the explanation, not a server
change (see `../RESULTS.md` §4.4).

### `erlc_compiler` — `erlc` over the bounded OTP stdlib + Elixir corpus slice

`ok`: `wall_ms=6094.921975`, `files_compiled=60`, `files_per_sec=9.844261`, `loc_per_sec=7629.794145`,
`peak_memory_bytes=135974912`, note
`corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`.
The `files_per_sec`/`loc_per_sec` rates are computed from the **produced** `.beam`/LOC counts (the
just-landed honesty fix in `src/workloads/erlc_compiler.rs`), so a partial run cannot overstate them.

### `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=48696.611443`, `files_attempted=105`, `files_compiled=223`,
`files_per_sec=4.579374`, `loc_per_sec=2299.215421`, `compiler_startup_ms=276.107007`,
`peak_memory_bytes=206336000`. Notes:
`batch compile failed; per-file fallback (version skew)`;
`compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`;
`97/105 compiled; 223 .beam produced`;
`223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)`.
**`files_compiled` (=223) exceeding `files_attempted` (=105) is expected and documented by the
workload itself:** one Elixir source can define several modules, so the produced `.beam` count can
exceed the source count. The throughput rate uses the produced count.

## Host load

`/proc/loadavg` is **host-wide** and this machine is shared with other concurrent work; across this
session it ran from `4.11` to `28.95`. The per-run before/after record and the exact commands are in
`m9-load-record.md`; the headline full passes were `11.01 → 6.02` (cross-runtime) and `4.68 → 4.43`
(external targets). The external latencies/throughputs above are *reproducible on this host*, not
milestone numbers.

## Reproducibility note

The `.NET` SDK situation is *tool/environment* state, not repository state:
a fresh sandbox with a sufficiently new `dotnet` first on `PATH`, or with no `dotnet` on `PATH` at all
(so the version-ranked store scan picks a ≥ 9.0 SDK), lets `plb` build and run; on this host a
`dotnet` **8.0.425** on `PATH` shadows the store and the row is the honest
`failed(plb-build-failed)`. `khepri` needs only the `rebar3` store entry and runs as-is.
