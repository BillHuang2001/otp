# m13 — external target workloads: what actually runs in this sandbox

Run date: 2026-10-15 21:03–21:10 UTC (= 2026-10-16 05:03–05:10 +0800). Commit measured: **`a141d6878c`**
(runtime+driver tree; the m13 measurement artifacts were committed as **`7801fabaea`**).
Host: 14 cores, **host-wide** `/proc/loadavg` (shared machine — see the load record beside this file).
Oracle: system `erl`, **OTP 29.1.1 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`).
`bench` = `erts_rust/target/release/bench` (release build of this crate at this commit).

Raw artifacts (committed beside this file): `m13-bench-external.json` (the four `--full` sys-only
targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`), `m13-bench-plb.json`
(the PLB run), `m13-bench-emqx.json` (the EMQX observation), and the 1-min load samples for the whole
batch (`m13-ext-load.log`). Diff vs m12: `m13-compare-vs-m12-external.txt`.

## Verdict

| target | status | metric measured |
|---|---|---|
| `erlc_compiler` | **ok** | `wall_ms=9679.61`; `files_per_sec=6.199`; `loc_per_sec=4804.22`; `files_compiled=60` |
| `elixir_compiler` | **ok** | `wall_ms=73993.35`; `files_per_sec=3.014`; `files_compiled=223`; `compiler_startup_ms=531.15` |
| `cowboy` | **ok** | `requests_per_sec=61363.95`; `latency_ms` p50 `0.145991` / p99 `2.160520`; `build_ms=59013.92` |
| `khepri` | **ok** | `wall_ms=89131.58`; `peak_memory_bytes=387690496` |
| `plb` | **ok** | `wall_ms=122157.47`; `test_time_ms` mean `1326.31` / p50 `700.51` / p99 `4513.28` (n=10) |
| `emqx` | `skipped(emqx-requires-otp-28)` | none — **no number fabricated** (`"metrics": []`, `"peak_memory_bytes": null`) |

**Five of the six external targets run; `emqx` skips** with the same verified mechanical reason as
m9/m10/m11/m12. All four `--full` rows in `m13-bench-external.json` are `ok` (`4/4`); `m13-bench-plb.json`
is one `ok` row; `m13-bench-emqx.json` is one `skipped` row.

**No source file under `/home/bill/Source/` was written** — the driver stages every build into a
scratch copy under `$TMPDIR` (the read-only checkouts stay clean).

> **Which runtime produces these numbers?** All six rows are executed by their **own system-ERTS
> toolchain** — `erlc` / `elixirc` / the Cowboy release on the oracle `erl` / `rebar3`'s Erlang / the
> `.NET` tool for PLB. **None is executed by the Rust runtime (`beam_rt`).** They are **reference**
> figures for the sys-only surfaces.

> **Host-load caveat (read this first).** The whole external-target batch ran in the window
> `21:03:00 → 21:09:30` UTC inside **one** load log (`m13-ext-load.log`), whose 1-min `/proc/loadavg`
> samples ran **min `69.46` / median `81.97` / max `99.87`** (27 samples at 15 s). That is roughly
> **8–10× the load** the m12 external pass saw (`10.91 → 9.34`), so **every** wall/latency/throughput
> figure below is inflated by CPU contention — and the m12→m13 deltas (`m13-compare-vs-m12-external.txt`)
> are a **record-vs-record, cross-load** comparison, not a capability change. Read the numbers as
> *reproducible on this host*, never as machine-speed constants.

## Environment findings

Evidenced **in the m13 artifacts**:

1. **`dotnet` on `PATH` is 9.0.318** (`/run/current-system/sw/bin/dotnet`) — `plb` runs with no `PATH`
   reordering: the version-aware resolver (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)`
   with min `9`) selects the greatest satisfying SDK and rejects the two 8.0.425 store entries.
   Provenance exactly as recorded in `m13-bench-plb.json`:
   ```
   build stage ok (dotnet --task build took 32871 ms)
   rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (nix-store): requires >= 9
   rejected dotnet 8.0.425 at /nix/store/qybc3wpz3hz5daipbmnj7rgc73hn35j2-dotnet-sdk-wrapped-8.0.425/bin/dotnet (nix-store): requires >= 9
   resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)
   ```
2. **`elixirc`/`mix` are 1.20.4** — `elixir_compiler` notes `compiler: Elixir 1.20.4 (compiled with
   Erlang/OTP 29)`.
3. **`erlc` is the oracle's `erlc`** — the embedded-module staging notes in `m13-bench-xrun.json`
   (`list_walk`, `copy_bound` and `gc_bound`) and in the A/B reports all name
   `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erlc`.
4. **`rebar3` is NOT on `PATH`** — `khepri` runs with no `PATH` prefix and no `$BENCH_TRY_NIX`, i.e.
   through the driver's version-ranked Nix-store scan. (The m13 `khepri` row records **no** tool note, so
   the resolved version is not restated here; at m12 the same path resolved `rebar3-3.27.1`.)
5. **The m13 `cowboy` build took 59 013.92 ms** (vs 42 527.11 ms at m12 and 39 512 ms at m10) — the
   relx release build (whose `erlang.mk` run git-fetches cowlib/ranch) plus the host load.

Carried over from the m12 record (**not re-verified in the m13 artifacts** — the m13 rows carry no note
for these): `clang 21.1.8` on `PATH`; `/usr/bin/time` absent (the driver's own per-child timers and
`/proc/<pid>` `VmHWM` polling are used); `$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` unset; the Cowboy
release's `foreground` start script `exec`s `erlexec`; and the `khepri` caveat that the store `rebar3`
escript's shebang points at an Erlang/OTP 28 `escript`, so it runs the Khepri suite under OTP 28 while
the row's `runtimes[]` entry describes the driver's resolved oracle (OTP 29) — the measured wall time is
real, the *reported* runtime version on a `SysOnly` tool-driven row describes the driver's runtime, and
no metric is fabricated.

## Per-target detail

### `erlc_compiler` — `erlc` over the bounded OTP stdlib + Elixir corpus slice — **`ok`**

```
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m13-bench-external.json --verbose
```
`ok`: `wall_ms=9679.605467`, `files_compiled=60`, `files_per_sec=6.198600`, `loc_per_sec=4804.224734`,
`peak_memory_bytes=130109440` (130.1 MB / 124.08 MiB), note
`corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`.
The `files_per_sec`/`loc_per_sec` rates are computed from the **produced** `.beam`/LOC counts, so a
partial run cannot overstate them. Versus m12 (same 60/60 files, unmodified system `erlc`): wall
6 842.18 → 9 679.61 ms (**+41.47 %**), `files_per_sec` 8.7691 → 6.1986 (**−29.31 %**) — i.e. the
throughput fell in proportion to the wall, in a run window whose 1-min load was ~8–10× m12's.

### `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus — **`ok`**

`ok`: `wall_ms=73993.345623`, `files_attempted=105`, `files_compiled=223`, `files_per_sec=3.013785`,
`loc_per_sec=1513.163097`, `compiler_startup_ms=531.154492`, `peak_memory_bytes=208650240`
(208.7 MB / 198.98 MiB). Notes:
`batch compile failed; per-file fallback (version skew)`;
`compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`;
`corpus = the checkout's unbuilt, read-only .ex sources staged into $TMPDIR; compiler = the installed
system elixirc, so a version skew with the checkout is expected`;
`97/105 compiled; 223 .beam produced`;
`223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)`.
**`files_compiled` (=223) exceeding `files_attempted` (=105) is expected and documented by the workload
itself** (one Elixir source can define several modules); the throughput rate uses the produced count.
Versus m12: wall 50 304.34 → 73 993.35 ms (**+47.09 %**), `files_per_sec` −32.02 %,
`compiler_startup_ms` 297.64 → 531.15 ms (**+78.45 %**) — again the load window, not a compiler change.

### `cowboy` — hello_world release + in-process HTTP/1.1 load (`--full`, 16 conns × 5 s) — **`ok`**

`ok`: `build_ms=59013.915121` (relx release built in a `$TMPDIR` scratch copy; the build's network fetch
of cowlib/ranch), `requests=307086`, `errors=0`, `requests_per_sec=61363.950413`,
`latency_ms` n=20000 min `0.025545` mean `0.267918` p50 `0.145991` p90 `0.520572` p99 `2.160520`
max `18.531975`, `peak_memory_bytes=160616448` (160.6 MB / 153.18 MiB).
Versus m12 the throughput **fell** 165 489.21 → 61 363.95 req/s (**−62.92 %**), the mean latency rose
153.55 % and the p99 rose 0.7424 → 2.1605 ms, while the number of completed requests fell
827 888 → 307 086 — the shared host's load is the explanation (this batch's window was
69.46 → 99.87), not a server change.

### `khepri` — Khepri `rebar3 eunit` in a scratch copy — **`ok`**

`ok`: `wall_ms=89131.579260`, `peak_memory_bytes=387690496` (387.7 MB / 369.73 MiB). `rebar3` resolves
through the driver's Nix-store scan, so the workload runs with no `PATH` prefix and no `$BENCH_TRY_NIX`.
Versus m12: 77 564.34 → 89 131.58 ms (**+14.91 %**) — within the load band, and the workload itself
carries the OTP-28-`rebar3` caveat recorded under "Environment findings" above.

### `plb` — Programming-Language-Benchmarks (dotnet) — **`ok`**

```
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb --out-file erts_rust/tools/bench/results/m13-bench-plb.json --verbose
```
`ok`: `wall_ms=122157.468348` (the two-stage `--task build` + `--task bench`; the build stage alone took
**32 871 ms**, per the row's note), `test_time_ms` n=10 min `0.0` mean `1326.310090` p50 `700.514150`
p90 `3637.365060` p99 `4513.283346` max `4610.607600`, `test_mem_bytes=90877952` (90.9 MB / 86.67 MiB),
`peak_memory_bytes=169918464` (169.9 MB / 162.05 MiB).
This is a **`--quick` observation on a shared host**, not a milestone figure: `test_time_ms` is PLB's own
per-test time across its 10 elixir kernels (`min` 0 = a sub-millisecond kernel) and `wall_ms` is dominated
by building the kernels' escripts. `peak_memory_bytes` is the driver's external `VmHWM` of the `.NET`
tool. **Read `p50` from the `p50` field — the row's *mean* is 1 326.31 ms while its *median* is
700.51 ms.** Versus m12: wall 70 648.46 → 122 157.47 ms (read manually from the two
`m13-bench-plb.json`/`m12-bench-plb.json` rows — no tool diff is archived for `plb`), i.e. **not** flat —
the m13 window was ~8× more loaded.

### `emqx` — EMQX umbrella build (make/mix through `env.sh`) — `skipped(emqx-requires-otp-28)`

`skipped`, `reason=emqx-requires-otp-28`, `"metrics": []`, `"peak_memory_bytes": null` — **no number is
fabricated for `emqx`**, and the driver accrued no build time (it skips immediately). The recorded cause,
verbatim from `m13-bench-emqx.json`:

> EMQX pins Erlang/OTP **28.5.0.7-2** (`env.sh:3` → `export OTP_VSN=28.5.0.7-2`; also
> `.tool-versions:1` → `erlang 28.5.0.7-2`), but the `erl` on `PATH`
> (`…/erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`) is OTP **29** (erts-17.1) — a combination EMQX does
> not build on. Two verified blockers: (1) the `:ezstd` dependency's NIF makefile generates its `env.mk`
> via `erl -noshell -s init stop -eval …` — an idiom OTP 29 halts **before** evaluating (verified:
> exit `0`, no output at all) — so `mix compile` fails with `No rule to make target '…/env.mk'`; and
> (2) even with that idiom patched the umbrella build needs host build tooling/system libraries absent
> here (the `:jq` NIF runs `autoreconf`/`configure` and needs `automake` + `libtool` + `cmake`, and
> `:sasl_auth` needs Kerberos headers — `krb5.h`).

The checkout pins OTP **28.5.0.7-2** at `env.sh:3` / `.tool-versions:1`; the host `erl` is OTP
**29.1.1 / erts-17.1**. The full enablement ladder (exact commands and each blocker's exact failing
output) is the previous batch's verified record **`m12-emqx-enablement-attempt.md`**, with the one-line
`:ezstd` `env.mk` idiom patch in `m12-emqx-ezstd-env-mk.patch` — **reference it, do not re-derive it**.
The pin lives in the read-only target and the host tooling gaps are independent of it, so the skip is
not satisfiable by a version override; swapping in a prepared toolchain/image is out of this driver's
scope. The gate reads only these declared pins and runs cheaply up front.

## Host load

`/proc/loadavg` is **host-wide** and this machine was shared with sibling work throughout the m13
session. The external batch's own log (`m13-ext-load.log`) records 27 samples over
`21:03:00 → 21:09:30` with 1-min load **min `69.46` / median `81.97` / max `99.87`**; the three sub-runs
completed at `21:06:59` (external, `--full`), `21:09:36` (plb) and `21:09:37` (emqx). For the whole
session's per-batch windows see `m13-load-record.md` §3; the session's only quiet window was the
35-second CLI probe (1-min load 20.11–23.63). The external latencies/throughputs above are
*reproducible on this host*, not milestone numbers.

## Reproducibility note

The `.NET` SDK situation is *tool/environment* state, not repository state: the version-aware resolver
(`tool::command_for_min`) picks the greatest `dotnet` ≥ 9.0 across `PATH` + `$BENCH_TOOL_PATH` + the Nix
store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH`
(verified again this pass: the two 8.0.425 store entries are rejected and the 9.0.318 on `PATH` is
chosen). `khepri` needs only the `rebar3` store entry and runs as-is. `emqx` remains a genuine toolchain
skip on any host without an OTP-28 Erlang that can pair with the Elixir/`mix` the umbrella build uses.

## Not measured in this pass (and why)

- **No Rust-runtime row for any of these targets — by construction.** All six are `Runtimes::SysOnly`,
  so under `--runtime rust` each reports **`skipped(runtime-unsupported)`**, never a number: the Rust
  runtime cannot boot an OTP release, run `rebar3`, or act as `erlc`/`elixirc`, so **the full target apps
  are never run under `beam_rt`**. The six workloads that *do* run under `beam_rt` are `baseline`,
  `slice`, `foreign`, `list_walk`, `copy_bound` and `gc_bound`, measured in `m13-bench-xrun.json` (see
  `m13-load-record.md`). Genuine end-to-end `beam_rt` execution is confined to those six.
- **PLB is `--quick` (`n=10` of its own kernels), not `--full`.** The workload's `--full` profile is not
  a milestone form here (see `../CONTEXT.md`); the run is an "it builds and runs" observation.
- **`cowboy` is a 5-second keep-alive load, not a sustained soak** — a single shared-host sample, with
  the p99 recorded rather than a long-tail distribution study.
- **`emqx` produced no measurement at all** — the honest skip above, with no build time accrued and no
  number recorded.

## See Also

- `m13-load-record.md` → the m13 pass's load record: every batch, its command, its `/proc/loadavg`
  window, the artifact it produced, the per-workload status, the matched-`N` A/B, the CLI probe and the
  tier tables.
- `m12-tool-targets-notes.md` / `m12-emqx-enablement-attempt.md` → the previous batch's target notes and
  the verified EMQX enablement ladder this skip references.
- `../CONTEXT.md` → the `bench` crate's shared conventions (workload table, probe/skip policy,
  `Runtimes::SysOnly`, result schema).
- `RESULTS.md` → the authoritative results summary (tables, reproduction commands, host-load record).
