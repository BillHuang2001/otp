# m10 — external target workloads: what actually runs in this sandbox

Run date: 2026-10-13 23:06–23:15 UTC (= 2026-10-14 +0800). Commit measured:
**`b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`**. Host: 14 cores, **host-wide** `/proc/loadavg`
(shared machine — see the load record beside this file). Oracle: system `erl`, **OTP 29 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/.../erl`).
`bench` = `erts_rust/target/release/bench` (release build of this crate at this commit).

Raw artifacts (committed beside this file): `m10-bench-external.json` (the four `--full` sys-only
targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`), `m10-bench-plb.json`
(the PLB run), `m10-bench-emqx.json` (the EMQX observation).

## Verdict

| target | status | metric measured |
|---|---|---|
| `khepri` | **ok** | `wall_ms=78762.66`; `peak_memory_bytes=363016192` |
| `cowboy` | **ok** | `requests_per_sec=210250.49`, `latency_ms` p50 `0.0636`, p99 `0.3020` |
| `erlc_compiler` | **ok** | `wall_ms=6785.46`, `files_per_sec=8.842` |
| `elixir_compiler` | **ok** | `wall_ms=47806.41`, `files_per_sec=4.665` |
| `plb` | **ok** | `wall_ms=70841.43`; `test_time_ms` p50 554.76 / p99 3716.01 (n=10) |
| `emqx` | `skipped(emqx-requires-otp-28)` | none — no number fabricated |

**Five of the six external targets run; `emqx` skips** with the same verified mechanical reason as m9.
**No source file under `/home/bill/Source/` was written** — the driver stages every build into a
scratch copy.

> **Which runtime produces these numbers?** All six rows are executed by their **own system-ERTS
> toolchain** — `erlc` / `elixirc` / the Cowboy release on the oracle `erl` / `rebar3`'s Erlang / the
> `.NET` tool for PLB. **None is executed by the Rust runtime (`beam_rt`).** They are **reference**
> figures for the sys-only surfaces (the Rust runtime cannot boot an OTP release, run `rebar3`, or act
> as `erlc`/`elixirc`, so every one of these workloads is `Runtimes::SysOnly`).

## Environment findings

1. **`dotnet` on `PATH` is now 9.0.318** (`/run/current-system/sw/bin/dotnet`) — so `plb` runs even
   without any `PATH` reordering: the version-aware resolver
   (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` with min `9`) selects the greatest
   satisfying SDK and rejects the two 8.0.425 store entries. Provenance as recorded:
   ```
   build stage ok (dotnet --task build took 21795 ms)
   rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (nix-store): requires >= 9
   rejected dotnet 8.0.425 at /nix/store/qybc3wpz3hz5daipbmnj7rgc73hn35j2-dotnet-sdk-wrapped-8.0.425/bin/dotnet (nix-store): requires >= 9
   resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)
   ```
   (m9's post-fix smoke proved the same resolution also holds with an 8.0.425 SDK forced first on
   `PATH`; the m10 run did not need to repeat that — its `PATH` already resolves ≥ 9 directly.)
2. **`clang` IS on `PATH`**: `clang version 21.1.8` (`/run/current-system/sw/bin/clang`).
3. **`rebar3` is NOT on `PATH`** but **resolves through the driver's version-ranked Nix-store scan**, so
   `khepri` runs with no `PATH` prefix and no `$BENCH_TRY_NIX`. The store entry is an escript whose
   shebang points at an Erlang/OTP 28 `escript`.
4. **`/usr/bin/time` is absent** — the driver's own per-child timers and `/proc/<pid>` `VmHWM` polling
   are used; no external timing tool participates.
5. **Network is available** — `cowboy`'s `erlang.mk` build git-fetched cowlib/ranch in ~40 s.

## Per-target detail

### `plb` — Programming-Language-Benchmarks (dotnet) — **`ok`**

`ok`: `wall_ms=70841.434786` (the two-stage `--task build` + `--task bench`; the build stage alone
took 21 795 ms), `test_time_ms` n=10 min `0.0` mean `1258.29` p50 `554.76` p90 `3544.79`
p99 `3716.01` max `3735.03`, `test_mem_bytes=92553216`, `peak_memory_bytes=170311680`.
This is a **`--quick` observation on a shared host**, not a milestone figure: `test_time_ms` is PLB's own
per-test time across its 10 elixir kernels (`min` 0 = a sub-millisecond kernel) and `wall_ms` is
dominated by building the kernels' escripts. `peak_memory_bytes` is the driver's external `VmHWM` of the
`.NET` tool. Read it as "PLB builds and runs on this host".

### `khepri` — Khepri `rebar3 eunit`

`ok`: `wall_ms=78762.659810`, `peak_memory_bytes=363016192`.
**Caveat worth recording:** the store `rebar3` is an escript whose shebang points at an Erlang/OTP 28
`escript`, so it runs the Khepri suite under **Erlang/OTP 28** while the report's `runtimes[]` entry
describes the driver's oracle (`erl` on `PATH`, OTP 29). The measured wall time is real, but the
*reported* runtime version on a `SysOnly` tool-driven row describes the driver's resolved runtime, not
necessarily the Erlang the external tool chose. No metric is fabricated.

### `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`) — `skipped(emqx-requires-otp-28)`

Unchanged from m9: the blocker is a genuine toolchain constraint, verified first-hand.
The checkout pins OTP 28 (`/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`;
`env.sh:3` → `export OTP_VSN=28.5.0.7-2`), the host `erl` is OTP 29.1.1 / erts-17.1.
The umbrella build aborts in the `:ezstd` dependency's NIF makefile, which generates its `env.mk` with
the idiom `erl -noshell -s init stop -eval '…'`; on OTP 29 `init stop` halts **before** the `-eval`
runs (demonstrated on this host: exit `0`, no output), so `mix compile` fails with
`No rule to make target '…/env.mk'`. The store `erl-28.5.0.7` cannot be paired with the OTP-29-built
Elixir 1.20.4/`mix` the umbrella build needs, and the pin lives in the read-only target, so it is not
satisfiable without a full toolchain swap (out of this crate's scope). The skip is immediate — no build
time accrued. No number is fabricated.

### `cowboy` — hello_world release + in-process HTTP/1.1 load (`--full`, 16 conns × 5 s)

`ok`: `build_ms=39511.73824` (relx release; network fetch of cowlib/ranch), `requests=1051576`,
`errors=0`, `requests_per_sec=210250.494864`, `latency_ms` n=20000 min `0.021357` mean `0.075663`
p50 `0.063634` p90 `0.099444` p99 `0.301994` max `3.622108`, `peak_memory_bytes=171356160`.
The p50 is essentially unchanged from m9 (`0.063633` vs `0.063911`) while throughput fell ~4.5 %
(`210 250` vs `220 144`) and p99 rose ~9 % — the shared host's load is the explanation, not a server
change (see `../RESULTS.md` §4.4 and §7.3).

### `erlc_compiler` — `erlc` over the bounded OTP stdlib + Elixir corpus slice

`ok`: `wall_ms=6785.45840`, `files_compiled=60`, `files_per_sec=8.842416`, `loc_per_sec=6853.332248`,
`peak_memory_bytes=135405568`, note
`corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`.
The `files_per_sec`/`loc_per_sec` rates are computed from the **produced** `.beam`/LOC counts, so a
partial run cannot overstate them. The m10 rate is ~10 % below m9's (`9.844`); both runs compiled the
same 60/60 files with the unmodified system `erlc`, so the move is host-load-attributable, not a
compiler change.

### `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=47806.412594`, `files_attempted=105`, `files_compiled=223`,
`files_per_sec=4.664610`, `loc_per_sec=2342.028875`, `compiler_startup_ms=262.045176`,
`peak_memory_bytes=208289792`. Notes:
`batch compile failed; per-file fallback (version skew)`;
`compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`;
plus the produced-count note.
**`files_compiled` (=223) exceeding `files_attempted` (=105) is expected and documented by the
workload itself:** one Elixir source can define several modules, so the produced `.beam` count can
exceed the source count. The throughput rate uses the produced count.

## Host load

`/proc/loadavg` is **host-wide** and this machine is shared with other concurrent work; across this
session it ran from `5.43` to `27.43`. The per-run before/after record and the exact commands are in
`m10-load-record.md`; the headline full passes were `5.43 → 7.59` (cross-runtime),
`7.94 → 8.03` (tier control) and `21.12 → 10.30` (external targets). The external
latencies/throughputs above are *reproducible on this host*, not milestone numbers.

## Reproducibility note

The `.NET` SDK situation is *tool/environment* state, not repository state: the version-aware resolver
(`tool::command_for_min`) picks the greatest `dotnet` ≥ 9.0 across `PATH` + `$BENCH_TOOL_PATH` + the Nix
store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH`
(proved in m9's defect-condition smoke). `khepri` needs only the `rebar3` store entry and runs as-is.
`emqx` remains a genuine toolchain skip on any host without an OTP-28 Erlang that can pair with the
Elixir/`mix` the umbrella build uses.
