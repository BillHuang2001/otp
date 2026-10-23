# m19 — external benchmark targets: activation evidence

Session 2026-10-23, host: `erl` = **OTP 29.1.1 / erts-17.1** (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1`),
`dotnet` = **9.0.318** on `PATH` (`/run/current-system/sw/bin/dotnet`), `rebar3` **absent from `PATH`** but
resolved from the Nix store by the shared store scan (`rebar3-3.27.3`), network available.
1-min load recorded before/after every pass (values per run below). No metric value was edited; the raw
`Report` JSONs are committed beside this file as `m19-target-{plb,khepri,emqx,ext}.json`.

## 1. `plb` — `ok` (activated, real numbers)

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p bench
erts_rust/target/release/bench run --quick --workloads plb --runtime sys \
  --out-file erts_rust/tools/bench/out/m19-plb.json     # → results/m19-target-plb.json
```

Load 1.40 → 1.82. Row: `wall_ms` **69 412 ms** (build stage 22 199 ms), `test_time_ms` over the **8**
kernels the tool timed (min 181 ms, mean 1 516 ms, p50 908 ms, max 3 441 ms), `test_mem_bytes` 95 424 512,
`peak_memory_bytes` 169 566 208. SDK provenance in the notes: `resolved dotnet 9.0.318 at
/run/current-system/sw/bin/dotnet (PATH)`, with both 8.0.425 store candidates named as rejected.

The route is the documented two-stage flow (`dotnet run -c Release --project tool -- --task build|bench
--langs elixir --no-docker`, cwd = the staged `bench/`), which the driver already drove; **elixir is the only
language PLB is driven with here** (the row now records that).

Two fixes to the driver (`src/workloads/plb.rs`):

* **`timeMS = 0` is an absence, not an instant.** PLB writes a result JSON per problem input regardless of
  whether the run was timed; when the child does not exit cleanly (here: aborted at the input's declared
  timeout) it records `Elapsed = 0` while still reporting the CPU time it consumed
  (`bench/tool/ProcessUtils.cs::MeasureAsync`: `m.Elapsed = TimeSpan.FromMilliseconds(0); if (ret >= 0) …`).
  Observed directly in the tool's log for `spectral-norm` 4000/8000: `time: 0ms … cpu-time: 5410ms`. Such
  results are now **excluded** from `test_time_ms` (previously they dragged its minimum to 0 and would read as
  an instant kernel) and each is named in a note; if *no* result carries a timing the row is
  `failed(plb-no-results)` exactly as for an empty result set.
* **Coverage notes**: the row records `langs=elixir (… --no-docker)`, the measured `test/input=time` list and
  the excluded inputs, so the multi-kernel aggregate is auditable.

## 2. `khepri` — `ok`

```sh
erts_rust/target/release/bench run --quick --workloads khepri --runtime sys \
  --out-file erts_rust/tools/bench/out/m19-khepri.json  # → results/m19-target-khepri.json
```

Load 1.33 → 1.18. Row: `wall_ms` **83 684 ms**, `peak_memory_bytes` 354 045 952. `rebar3` resolved by the
store scan (no `PATH` prefix, no `$BENCH_TRY_NIX`) to
`/nix/store/hmvrwzxq25f7ps0lnm62d6qqhiapfdwp-rebar3-3.27.3/bin/rebar3`; the suite reported
**`1501 tests, 0 failures`** (`Finished in 75.273 seconds`), captured in the row's notes.

The driver now records **which Erlang the suite ran under**: `rebar3` is an escript whose shebang names
`/nix/store/y5db8dvib9fr41l53qrhgzpgx6sbb04b-erlang-29.1.2/bin/escript`, i.e. **OTP 29.1.2** — close to, but
not identical with, the driver's oracle (`erl` on `PATH`, OTP 29.1.1) that the report's `runtimes[]` row
describes. (The first implementation read the whole file with `fs::read_to_string`, which fails on an escript's
binary body and misreported every escript as shebang-less; it now reads the head only and decodes lossily,
pinned by a test with a non-UTF-8 body.) A manual run of the same command in a `$TMPDIR` copy reproduced
`exit=0`, `1501 tests, 0 failures`, `Finished in 76.515 seconds` — the suite genuinely runs.

## 3. `emqx` — `skipped(emqx-requires-otp-28)` (re-proved)

```sh
erts_rust/target/release/bench run --quick --workloads emqx --runtime sys \
  --out-file erts_rust/tools/bench/out/m19-emqx.json   # → results/m19-target-emqx.json
```

Load 1.18. Pin: `.tool-versions:1` (`erlang 28.5.0.7-2`) / `env.sh:3` (`export OTP_VSN=28.5.0.7-2`).
Host `erl` = OTP 29.1.1. The two blockers, re-verified this session:

**(1) the `:ezstd` `env.mk` idiom is OTP-29-specific — and an OTP-28 `erl` DOES avoid it.** An OTP-28 runtime is
reachable in the store (`/nix/store/5y0714j8czhkjbmjb6l6m46n5w5fyr6q-erlang-28.5.0.5/bin/erl`, prints `28`):

```
$ erl        -noshell -s init stop -eval 'io:format("X~n")'   # OTP 29.1.1 → exit 0, NO output
$ …-erlang-28.5.0.5/bin/erl -noshell -s init stop -eval 'io:format("X~n")'   # OTP 28 → prints X, exit 0
```

Note the store holds **28.5.0.5**, not the pinned 28.5.0.7-2, and the pin also names
`elixir 1.19.1-otp-28` (host: Elixir 1.20.4 compiled with OTP 29).

**(2) the host build toolchain is still missing, independently of the Erlang release.**

```
$ command -v automake libtool cmake   → none of them on PATH
$ ls -d /nix/store/*-automake-* /nix/store/*-libtool-* /nix/store/*-cmake-*  (built outputs)
    → only .drv/.patch/.tar.gz entries: automake and cmake are NOT built in the store
$ ls /nix/store/*-krb5-1.22.2-dev/include/krb5.h → present, but only as a store -dev output,
    nowhere on the default include path
```

So `:jq`'s `autoreconf`/`configure` (needs `automake` + `libtool`) and `:rocksdb`/`:quicer` (`cmake`) cannot
build, and `:sasl_auth` cannot find `krb5.h`. This matches the m12 ladder
(`m12-emqx-enablement-attempt.md`), whose later rungs reached exactly `krb5.h`.

**Decision:** per-workload OTP-28 support was *not* implemented. Blocker (2) is release-independent, so an
OTP-28 oracle would only convert the honest skip into a `failed(emqx-build-failed)` while adding a
host-specific toolchain override to the driver. The gate note now says all of this — that blocker (2) is
independent of the Erlang release, that an OTP-28 store `erl` does evaluate the idiom, and which of
`automake`/`libtool`/`cmake` are absent (re-probed on every run via `absent_build_tools()`, so installing one
changes the note instead of leaving a stale claim). The skip reason string is unchanged.

## 4. Regression check on the targets that already ran — all `ok`

```sh
erts_rust/target/release/bench run --quick \
  --workloads cowboy,erlc_compiler,elixir_compiler --runtime sys \
  --out-file erts_rust/tools/bench/out/m19-ext-check.json   # → results/m19-target-ext.json
```

Load 1.08 → 1.13. `cowboy` (real relx release, network git-fetch of cowlib/ranch): build 43 283 ms,
**242 117 requests / 0 errors / 120 995 req/s**, `latency_ms` p50 0.063 ms / p99 0.127 ms, peak RSS
154 460 160. `erlc_compiler`: 9.837 files/s over 60 files (7 624 loc/s), peak RSS 131 407 872.
`elixir_compiler`: 3.317 files/s (39 files, per-file fallback mode), compiler startup 290.8 ms, peak RSS
200 458 240.

## Reading these numbers

As always on this shared host: the ratios/counters are the load-robust reading, the absolute wall times are
not (the loads above are recorded per pass). Nothing here is a rust-vs-sys comparison — all four rows are
`Runtimes::SysOnly`.
