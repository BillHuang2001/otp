# m6 — external target workloads: what actually runs in this sandbox

Run date: 2026-10-11 (UTC). Host: 14 cores, **host-wide** `/proc/loadavg` (shared machine — see
"Host load" below). Oracle: system `erl`, **OTP 29 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/.../erl`).
`bench` = `erts_rust/target/release/bench` (release build of this crate at this commit).

Raw artifacts (committed beside this file): `m6-bench-plb.json`, `m6-bench-khepri.json`,
`m6-bench-emqx.json`, `m6-bench-cowboy.json`, `m6-bench-erlc-compiler.json`,
`m6-bench-elixir-compiler.json`, `m6-bench-tool-targets.json` (the three tool-driven targets in one
report, produced with `BENCH_TRY_NIX=1`).

## Verdict

| target | status | metric measured |
|---|---|---|
| `plb` | **ok** (after fixing the environment — see below) | `wall_ms=125424.71` / `82557.97`; `test_time_ms` mean `1474.44` / `1327.90` (n=10) |
| `khepri` | **ok** | `wall_ms=101386.88` / `81362.09` |
| `emqx` | `skipped(emqx-requires-otp-28)` | none — no number fabricated |
| `cowboy` | **ok** | `requests_per_sec=40684.14`, `latency_ms` mean `0.192`, p99 `1.628` |
| `erlc_compiler` | **ok** | `wall_ms=9621.48`, `files_per_sec=6.236` |
| `elixir_compiler` | **ok** | `wall_ms=21709.84`, `files_per_sec=1.796` |

**No harness bug was found; no source file was changed.** The one real blocker was an *environment*
one (a missing .NET 9 SDK in `/nix/store`) and it was fixed by realizing the SDK, not by editing code.

## Environment findings (context in the task was stale on two points)

1. `rebar3` **is** in this sandbox's store:
   `/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin/rebar3` (reports
   `rebar 3.27.1 on Erlang/OTP 28 Erts 16.4.0.6`). Neither `rebar3` nor `dotnet` is on `PATH`
   and `$BENCH_TOOL_PATH` is unset, so both are supplied by **resolution step 3, the Nix-store
   scan** — not `PATH`, not `$BENCH_TOOL_PATH`, not the `nix` shim.
2. `dotnet` was **not** in the store initially (`plb` → `probe_ready:false / "dotnet-absent"`).
   The SDK is needed because the PLB tool and driver project target .NET 9
   (`bench/tool/BenchTool.csproj` → `<TargetFramework>net9</TargetFramework>`,
   `bench/include/dotnet/app.csproj` → `<TargetFrameworks>net9;</TargetFrameworks>`).

Realized into the store to make the target run (the driver was not touched):

```
nix build --no-link --print-out-paths nixpkgs#dotnet-sdk      # dotnet-sdk-wrapped-8.0.425  (~3.2 GB closure, <45 s)
nix build --no-link --print-out-paths nixpkgs#dotnet-sdk_9    # dotnet-sdk-wrapped-9.0.318  (~3.2 GB closure, ~2 min)
```

Because the store scan ranks by the version named in the entry, `9.0.318 > 8.0.425`, the scan now
resolves `dotnet` to a 9.x SDK (`/nix/store/abbkzygy9aix7b6m9vqlm1xw766mlrf1-dotnet-sdk-9.0.318/bin/dotnet`,
`--version` → `9.0.318`). Both `dotnet` binaries in the store (`…-dotnet-sdk-9.0.318` and
`…-dotnet-sdk-wrapped-9.0.318`) run correctly.

### Probe output before vs after (`bench list --json`)

* **Initial store state** (no `dotnet`): the only difference between the two environments was
  `plb` — `probe_ready:false / "dotnet-absent"` without `BENCH_TRY_NIX`, `probe_ready:true / null`
  with it (the documented `available()` behaviour: the opt-in flag asserts availability without
  validating the shim). Every other workload probed identically.
* **Final store state** (both SDKs present): the two outputs are **byte-identical** — every
  workload probes `ready`, because the store scan resolves both tools locally before the shim is
  ever considered. `BENCH_TRY_NIX=1` therefore had **no effect** on any m6 run.

## Per-target detail

### `plb` — Programming-Language-Benchmarks (dotnet)

* Tool: `dotnet` from the store scan (step 3).
* **First attempt (SDK 8.0.425, before realizing the 9.x SDK) → `failed(plb-build-failed)`**,
  exit `Some(1)`, captured stdout:
  `/nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/share/dotnet/sdk/8.0.425/Sdks/Microsoft.NET.Sdk/targets/Microsoft.NET.TargetFrameworkInference.targets(166,5): error NETSDK1045: The current .NET SDK does not support targeting .NET 9.0. Either target .NET 8.0 or lower, or use a version of the .NET SDK that supports .NET 9.0.`
  The recorded *store path* is the direct evidence that the store scan, not `PATH`, supplied the tool.
  This is an environment/version constraint (missing 9.x SDK) and was recorded, not papered over.
* **After realizing `nixpkgs#dotnet-sdk_9` → `ok`**:
  `wall_ms=125424.707806`, `peak_memory_bytes=169779200`, `test_time_ms` n=10 mean `1474.4409`,
  p50 `981.1295`, p99 `4779.909864`/`4860.882` (max), `test_mem_bytes=93110272`, note
  `build stage ok (dotnet --task build took 59730 ms)`.
* The combined `BENCH_TRY_NIX=1` run reproduced it independently: `wall_ms=82557.967877`, build
  `22881 ms`, `test_time_ms` mean `1327.90009`, p50 `639.5317`, p99 `3723.841034`, peak
  `171868160`. (The two runs differ ~35 % in wall time; the PLB `--task build` half is the volatile
  part and the host load differed — see "Host load".)

### `khepri` — Khepri `rebar3 eunit`

* Tool: `rebar3` from the store scan (step 3). Ran `ok`: `wall_ms=101386.875017`,
  `peak_memory_bytes=350093312` (standalone run); `wall_ms=81362.08899`,
  `peak_memory_bytes=376324096` (combined run).
* **Caveat worth recording:** the store `rebar3` is an escript whose shebang is
  `#!/nix/store/qlclzl3234h2jwjpvy2gs72vzq2g6wkw-erlang-28.5.0.7/bin/escript`, i.e. it runs the
  Khepri suite under **Erlang/OTP 28** while the report's `runtimes[]` entry describes the driver's
  oracle (`erl` on `PATH`, OTP 29). The measured wall time is real, but the *reported* runtime
  version on a `SysOnly` tool-driven row describes the driver's resolved runtime, not necessarily
  the Erlang that the external tool chose to run. No metric is fabricated.

### `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`)

`skipped(emqx-requires-otp-28)`. The blocker is a genuine toolchain constraint, verified first-hand:

* The checkout pins OTP 28: `.tool-versions` → `erlang 28.5.0.7-2`; `env.sh` → `OTP_VSN=28.5.0.7-2`.
  The host `erl` is `otp_release=29`, `symbolic version=17.1`.
* The mechanism, demonstrated directly on this host (exit `0`, **no output at all**):
  ```
  $ erl -noshell -s init stop -eval 'io:format("ENV_MK_WOULD_BE_WRITTEN~n"), halt().'
  (no output)   exit=0
  ```
  `init stop` halts before `-eval` is evaluated, so the EMQX dependency `:ezstd`'s makefile — which
  generates its `env.mk` with exactly this idiom — writes nothing, and EMQX's `mix compile` then
  aborts with `No rule to make target '…/env.mk'`.
* `deps/ezstd` is not present in the read-only checkout (deps are not pre-fetched), so the wording
  of that dependency's makefile was not re-read; the idiom's failure is what was reproduced here.
* Swapping in a whole OTP-28 toolchain (the store `erl-28.5.0.7` cannot be paired with the
  OTP-29-built Elixir 1.20.4/`mix` on `PATH`) is out of this crate's scope, as the workload's own
  module docs state. Nothing was changed to paper over it.

### `cowboy` — hello_world release + in-process HTTP/1.1 load

`ok`: `build_ms=66938.6673` (relx release; network fetch of cowlib/ranch), `requests=81412`,
`errors=0`, `requests_per_sec=40684.1421`, `latency_ms` n=20000 min `0.024528` mean `0.192007`
p50 `0.101487` p90 `0.348004` p99 `1.628060` max `7.669393`, `peak_memory_bytes=143941632`.

### `erlc_compiler` — `erlc` over the bounded OTP stdlib + Elixir corpus slice

`ok`: `wall_ms=9621.477318`, `files_compiled=60`, `files_per_sec=6.236049`, `loc_per_sec=4833.249455`,
`peak_memory_bytes=130744320`, note `corpus: 60 files from stdlib+elixir`.

### `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=21709.839501`, `files_attempted=25`, `files_compiled=39`,
`files_per_sec=1.796420`, `loc_per_sec=1114.379496`, `compiler_startup_ms=679.528277`,
`peak_memory_bytes=190021632`. Notes: `batch compile failed; per-file fallback (version skew)`;
`compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`; `24/25 compiled; 39 .beam produced`.

## Host load

`/proc/loadavg` is **host-wide** and this machine is shared with other concurrent work; it swung
between `3` and `>600` during this session, so the absolute latencies/throughputs below are
*reproducible on this host*, not milestone numbers. Recorded immediately before and after every run
(1-minute average):

| run | before | after |
|---|---|---|
| `bench list` probe diff | `8.44 3.99 5.94` | — |
| `khepri` | `131.72 39.14 17.74` | `637.80 277.96 107.48` |
| `emqx` | `602.98 276.72 107.99` | `559.90 273.20 107.76` |
| `plb` (SDK 8.0.425 → failed) | `559.90 273.20 107.76` | `526.28 270.99 107.93` |
| `plb` (SDK 9.0.318 → ok) | `475.17 337.95 155.09` | `490.82 411.25 217.89` |
| `cowboy` | `490.82 411.25 217.89` | `471.58 421.99 235.66` |
| `erlc_compiler` | `474.34 423.39 237.11` | `486.28 427.57 240.46` |
| `elixir_compiler` | `486.28 427.57 240.46` | `508.76 437.07 248.55` |
| `plb,khepri,emqx` (`BENCH_TRY_NIX=1`) | `503.33 437.13 249.59` | `429.84 432.30 281.54` |
| `cargo test -p bench` | `427.63 431.74 282.97` | — |

Runs were executed **strictly one at a time** (never two benchmark workloads concurrently).

## Commands run

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
erts_rust/target/release/bench list --json                                   # and with BENCH_TRY_NIX=1
nix build --no-link --print-out-paths nixpkgs#dotnet-sdk                     # realize .NET 8 SDK
nix build --no-link --print-out-paths nixpkgs#dotnet-sdk_9                   # realize .NET 9 SDK (what plb needs)
erts_rust/target/release/bench run --quick --runtime sys --workloads plb,khepri,emqx --out-file erts_rust/tools/bench/results/m6-bench-tool-targets.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads khepri           --out-file erts_rust/tools/bench/results/m6-bench-khepri.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads plb              --out-file erts_rust/tools/bench/results/m6-bench-plb.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx             --out-file erts_rust/tools/bench/results/m6-bench-emqx.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads cowboy           --out-file erts_rust/tools/bench/results/m6-bench-cowboy.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads erlc_compiler    --out-file erts_rust/tools/bench/results/m6-bench-erlc-compiler.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads elixir_compiler  --out-file erts_rust/tools/bench/results/m6-bench-elixir-compiler.json --verbose
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench
```

`cargo test -p bench`: **213 + 4 + 1 + 3 passed, 0 failed** (including the real-store scan test
`the_real_store_scan_prefers_the_newest_installed_version`, which now sees both `dotnet` 8.0.425 and
9.0.318 and still picks the greatest version first).

## Reproducibility note

The `.NET` SDKs are *store* state, not repository state: a fresh sandbox that has not realized
`nixpkgs#dotnet-sdk_9` will again report `plb` → `failed(plb-build-failed)` (with `NETSDK1045`) even
though `probe()` reads ready. That is the honest, un-fabricated outcome for that environment; the
committed artifacts above were produced with the 9.x SDK present. `khepri` needs only the
`rebar3` store entry and runs as-is.
