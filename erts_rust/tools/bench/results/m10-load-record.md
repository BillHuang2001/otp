# m10 measurement pass — load record & results pointer

Measurement-and-archive record for the **m10** pass (the full cross-runtime run of the six
`Runtimes::Both` workloads under both runtimes, the same pass with the interpreter tier pinned, and the
full sys-only external-target run).
Facts only: the exact commands, the `/proc/loadavg` observed before/after each run, the commit measured,
and the sys-vs-rust ratio per workload.
The numbers themselves live in the committed artifacts beside this file:

- `m10-bench-xrun.json` — the full cross-runtime `Report` (schema_version 1, profile `full`).
- `m10-bench-xrun-interp.json` — the same six workloads, `--runtime rust` only, with
  `BEAM_RT_TIER=interpreter` pinned (the **Rust-vs-Rust tier-effect** control — it contains **no** `sys`
  rows and is NOT a system-ERTS comparison).
- `m10-compare-cross-runtime.txt` — `bench compare` for the cross-runtime report (sys vs rust inside one file).
- `m10-compare-tier-effect.txt` — `bench compare m10-bench-xrun-interp.json m10-bench-xrun.json`
  (Rust interpreter → Rust default; `a` = interpreter).
- `m10-compare-vs-m9-xrun.txt` / `m10-compare-vs-m9-external.txt` — `bench compare` against the m9 record.
- `m10-bench-external.json` — the sys-only external targets in one `--full` report
  (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`).
- `m10-bench-plb.json` — `ok` (the version-aware tool resolution picks `dotnet` 9.0.318).
- `m10-bench-emqx.json` — `skipped(emqx-requires-otp-28)`.

> **Commit measured.** Everything below was measured at **`b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`**
> (`b4593818b3`, the M10 milestone head: "docs: milestone-10 CHANGE.md entry, the M9 ratio correction,
> and the post-M10 root-owned figures"). The driver **and** `beam_rt` were both built from this tree
> (`--release -p beam_rt -p bench`) immediately before the pass. A sibling agent may rebuild
> `beam_rt`/`beam_engine` concurrently, so re-record the commit if the figures are re-run.

> **Host-load caveat (read this first).** The benchmark host is a shared, usually-oversubscribed
> 14-core machine. During this session the 1-minute `/proc/loadavg` ran **from 5.43 to 27.43**
> (see the record below). Every **absolute latency/wall/throughput** figure in the artifacts is
> therefore **inflated** by CPU contention when the load was high and must not be read as a
> machine-speed constant. The load-robust signals are the **per-op / per-element / per-call marginals**
> (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counts**
> (`*_ops`, `*_elements`); the per-runtime loop count `N` is fixed per case, so the per-unit figures are
> the comparable ones across the two runtimes.
> **A runtime-major report can be biased by drifting load:** the runner emits all `sys` rows first,
> then all `rust` rows, so if the load drifts during a run the two sides are measured in different
> conditions. The full cross-runtime pass below rose from 5.43 to 7.59, i.e. the later (`rust`) side ran
> under a *heavier* load — read the ratios, not the absolute pair.

## 1. Environment

- Commit measured: `b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`.
- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1) at
  `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in the report.
- Host: NixOS 26.11 (Zokor), Linux 7.2.9, `nproc` = 14, 62.29 GiB RAM.
- Toolchain: `rustc 1.98.1`, `cargo 1.98.0`; `clang` 21.1.8 on `PATH`.
- `dotnet` **9.0.318** is on `PATH` (`/run/current-system/sw/bin/dotnet`); the store also holds two
  8.0.425 SDKs, which the version-aware resolver rejects for `plb` (min `9`).
- `rebar3` is **not** on `PATH` — resolved from the Nix store by the driver's store scan.
- `elixirc` **1.20.4** (compiled with Erlang/OTP 29) — the installed system compiler.
- `$HOME` is read-only; `/usr/bin/time` is **absent** (the driver's own timers and `/proc/<pid>`
  `VmHWM` polling are used).

## 2. Exact commands

```sh
# build (once; the wrapper supplies a writable CARGO_HOME — $HOME is read-only)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# probe status
erts_rust/target/release/bench list --json

# FULL cross-runtime run — the six Runtimes::Both workloads, --full, both runtimes (wall 2m36s)
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m10-bench-xrun.json --verbose

# FULL tier-effect control — the same workloads, rust only, interpreter pinned (wall 41s)
BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --full --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m10-bench-xrun-interp.json

# focused stability re-checks (not archived): copy_bound under both tiers, and foreign under rust
erts_rust/target/release/bench run --quick --runtime rust --workloads copy_bound
BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --quick --runtime rust --workloads copy_bound
erts_rust/target/release/bench run --quick --runtime rust --workloads foreign

# FULL sys-only external targets (one report; wall 3m00s)
erts_rust/target/release/bench run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/out/m10-bench-external.json --verbose

# the two tool-driven targets (quick, one workload each)
erts_rust/target/release/bench run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/out/m10-bench-plb.json  --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/out/m10-bench-emqx.json --verbose
```

Workloads were run **serially** (no two benchmark workloads at once). The artifacts were written to the
gitignored `out/` and then archived into `results/` with only the two mechanical path normalisations
(the run-host worktree prefix in `runtimes[].program` → `erts_rust/target/release/beam_rt`; a scratch
`$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

## 3. `/proc/loadavg` record (1-min value first, in the order the passes were run)

| pass | before (1-min) | after (1-min) | artifact |
|---|---|---|---|
| **FULL cross-runtime pass** (wall 2m36s) | `5.43` | `7.59` | `m10-bench-xrun.json` |
| **FULL tier-effect control** (`rust` only, wall 41s) | `7.94` | `8.03` | `m10-bench-xrun-interp.json` |
| focused `copy_bound` default (quick) | `10.18` | `10.81` | (not archived) |
| focused `copy_bound` interpreter (quick) | `10.81` | `10.74` | (not archived) |
| focused `foreign` rust (quick) | `10.74` | `10.74` | (not archived) |
| **FULL external-targets pass** (wall 3m00s) | `21.12` | `10.30` | `m10-bench-external.json` |
| PLB quick pass (wall 1m34s) | `10.04` | `27.43` | `m10-bench-plb.json` |
| EMQX quick pass (skips immediately) | `27.43` | `27.43` | `m10-bench-emqx.json` |

The archived artifacts' own `generated_at_unix_ms` (UTC) bookend the session: `m10-bench-xrun.json`
**23:08:46**, `m10-bench-xrun-interp.json` **23:09:48**, `m10-bench-external.json` **23:13:19**,
`m10-bench-plb.json` **23:14:56**, `m10-bench-emqx.json` **23:14:57**. The session span is
**2026-10-13 23:06–23:15 UTC** (= 2026-10-14 07:06–07:15 +0800, the date the M10 milestone entry
carries). The 1-min load across the session was 5.43 → 27.43.

## 4. Per-workload status (full cross-runtime pass, `m10-bench-xrun.json`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust |
|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` |
| `slice`      | compute   | `ok` | `ok` |
| `foreign`    | compute   | `ok` | `ok` |
| `list_walk`  | compute   | `ok` | `ok` |
| `copy_bound` | messaging | `ok` | `ok` |
| `gc_bound`   | gc        | `ok` | `ok` |

No rust row is fabricated: every rust row carries real `Summary`/scalar metrics (no `null`-only
rows, no `unsupported` classification, no crash/exit-3). Every case validated its own copy round-trip
with a bound-variable pattern on both sides and printed the deterministic constant (recorded in the
report's `observed stdout:` notes), so the two runtimes executed identical work.

The tier-effect control (`m10-bench-xrun-interp.json`) is **6 rust rows, all `ok`** (no `sys` rows by
construction). The full external pass (`m10-bench-external.json`) is **4 rows, all `ok`**.
`m10-bench-plb.json` is **1 `ok`** row; `m10-bench-emqx.json` is **1 `skipped`** row (no metric).

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`rust/sys` > 1 means the Rust side costs **more** per unit (its ns value is higher). The marginals
(`*_ns_*`) are the load-robust cross-runtime signal; the wall/startup rows are inflated by the load
above and are shown for completeness. **Both sides of every ratio are named** — `sys` is the system
ERTS, `rust` is `beam_rt` at its default (baseline-JIT) tier.

| workload | headline cross-runtime metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 114.3603 ms | 15.5243 ms | **0.136×** (rust 7.37× faster) |
| `baseline`   | `vm_memory_total_bytes` | 36 229 856 B | 805 458 B | **0.022×** |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 135.08–141.48 ms | 15.38–48.12 ms | **0.109×–0.352×** (rust faster) |
| `foreign`    | `case_<name>_ns_per_call` | 210.88–339.42 ns | 16 326.00–22 566.61 ns | **52.4×–78.7×** (rust higher) |
| `list_walk`  | `case_list_walk_ns_per_element` | 2.1431 ns | 47.3880 ns | **22.1×** (rust higher) |
| `copy_bound` | `case_<name>_ns_per_op` | 240.47–1 453.98 ns | 3 267.85–79 518.12 ns | **13.6×–54.7×** (rust higher) |
| `gc_bound`   | `case_churn_ns_per_op` | 4 364.91 ns | 147 194.37 ns | **33.7×** (rust higher) |

### 5.1 Per-case detail

`baseline` (whole-process cost — startup latency is the trustworthy cross-runtime comparison):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `startup_ms` (mean) | 114.3603 | 15.5243 | 0.136× |
| `compute_loop_ms` (mean) | 190.7154 | 60.9056 | 0.319× |
| `compute_ops_per_sec` | 5 243 414.77 | 16 418 845.32 | 3.13× (rust faster; native floor, not interpreter throughput) |
| `vm_memory_total_bytes` | 36 229 856 | 805 458 | 44.98× |
| `peak_memory_bytes` (RSS) | 136 417 280 | 31 612 928 | 4.32× |

`slice` (one call per process: `case_<name>_wall_ms` mean, and the comparable per-process memory):

| case | sys wall_ms | rust wall_ms | rust/sys | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|
| `add` | 138.6098 | 15.3755 | 0.111× | 66.33 | 14.61 |
| `fib` | 135.8764 | 16.2162 | 0.119× | 66.71 | 15.21 |
| `pick` | 135.0812 | 16.0970 | 0.119× | 66.61 | 15.18 |
| `calendar_last_day` | 136.0588 | 18.6556 | 0.137× | 66.25 | 15.93 |
| `queue_from_size` | 140.3566 | 26.0005 | 0.185× | 66.54 | 17.95 |
| `gb_trees_size` | 135.9206 | 25.1163 | 0.185× | 66.29 | 16.00 |
| `base64_encode_size` | 141.4838 | 26.6790 | 0.189× | 66.50 | 17.15 |
| `ordsets_from_size` | 135.3315 | 26.1457 | 0.193× | 66.31 | 15.78 |
| `string_str_length` | 136.5086 | 48.1240 | 0.352× | 66.29 | 26.32 |

`slice` whole-workload `peak_memory_bytes`: sys 71 868 416 → rust 27 848 704 (**2.58×** lower for rust).

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys | sys startup p50 (ms) | rust startup p50 (ms) | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|---|
| `bstr_to_lower` | 339.4247 | 17 789.3946 | 52.41× | 132.73 | 15.24 | 66.40 | 73.76 |
| `bstr_capitalize_token` | 311.8324 | 22 566.6138 | 72.36× | 134.55 | 16.57 | 66.46 | 76.40 |
| `emqx_base62_encode` | 210.8817 | 16 601.0457 | 78.72× | 132.18 | 16.24 | 66.62 | 61.84 |
| `emqx_base62_decode` | 293.9290 | 16 325.9957 | 55.55× | 120.27 | 16.55 | 66.28 | 62.95 |

`list_walk` (BIF-free interpreted list walk; startup-cancelled per element):

| metric | sys | rust |
|---|---|---|
| `case_list_walk_ns_per_element` | 2.1431 ns | 47.3880 ns (**22.11×** higher) |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 |
| `case_list_walk_startup_ms` (mean) | 140.5296 | 16.7908 |
| `case_list_walk_peak_rss_bytes` (p50) | 66.43 MiB | 15.32 MiB |

`copy_bound` (ETF / ETS / send-receive copy boundaries; startup-cancelled `ns_per_op`):

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust (deterministic) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary`/`binary_to_term` | 651.9348 | 9 838.0045 | 15.09× | 1 500 000 / 5 000 | 19.25 |
| `ets_copy` | `ets:insert`+`ets:lookup` | 240.4679 | 3 267.8549 | 13.59× | 2 000 000 / 100 000 | 90.30 |
| `send_recv` | two-process send/receive copy | 1 453.9765 | 79 518.1199 | 54.69× | 500 000 / 2 000 | 20.99 |

`gc_bound` (allocate-and-abandon churn; startup-cancelled `ns_per_op`):

| metric | sys | rust |
|---|---|---|
| `case_churn_ns_per_op` | 4 364.9104 ns | 147 194.3719 ns (**33.72×** higher) |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 |
| `case_churn_startup_ms` (mean) | 125.9192 | 16.3142 |
| `case_churn_peak_rss_bytes` (p50) | 66.77 MiB | **15.40 MiB** |

> `gc_bound`'s row measures the allocate-and-abandon **wall time**; whether a runtime's collector
> actually runs during the loop is its own admission policy and is not claimed here (the report's
> own note says the same). The rust peak-RSS figure is the one that moved most since m9 (38.78 → 15.40
> MiB), i.e. the BIF-result copy-out + pooled-heap rewind bounds this loop's memory.

## 6. The tier effect (Rust default vs Rust `BEAM_RT_TIER=interpreter`) — NOT a system-ERTS comparison

Both columns are the **same Rust runtime** (`beam_rt`); only the execution tier differs. `interp/default`
> 1 means the default baseline-JIT tier is **cheaper** than the interpreter. Figures are the report
> `mean` over the `--full` run (`m10-bench-xrun.json` rust default vs `m10-bench-xrun-interp.json`).

| workload | metric | default | interpreter | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 15.5243 | 15.8192 | 1.019× |
| `baseline` | `compute_loop_ms` | 60.9056 | 59.0437 | 0.969× |
| `foreign` | `bstr_to_lower` ns/call | 17 789.39 | 21 959.53 | 1.234× |
| `foreign` | `bstr_capitalize_token` ns/call | 22 566.61 | 27 023.90 | 1.198× |
| `foreign` | `emqx_base62_encode` ns/call | 16 601.05 | 20 292.12 | 1.222× |
| `foreign` | `emqx_base62_decode` ns/call | 16 325.99 | 23 850.45 | 1.460× |
| `list_walk` | `ns_per_element` | 47.3880 | 181.9423 | **3.839×** |
| `copy_bound` | `etf_roundtrip` ns/op | 9 838.00 | 10 676.66 | 1.085× |
| `copy_bound` | `ets_copy` ns/op | 3 267.85 | 4 165.65 | 1.275× |
| `copy_bound` | `send_recv` ns/op | 79 518.12 | 36 969.39 | **0.465×** (default 2.15× *dearer*) |
| `gc_bound` | `churn` ns/op | 147 194.37 | 291 856.40 | 1.983× |

**Reading.** The baseline JIT is a clear win for `list_walk` (3.84×), `gc_bound churn` (1.98×) and the
`foreign` per-call rows (1.20×–1.46×), and neutral for `startup`/`compute`/`etf`. The one **inverted**
row is the two-process **`send_recv`** loop: the default (baseline-JIT) tier is **2.15× dearer** than the
interpreter, and a focused `--quick` re-run reproduced it more strongly (default **97 517.77** vs
interpreter **29 492.93** ns/op ⇒ 3.31×). At the previous HEAD `PERF.md`'s own `CURRENT` snapshot recorded
the same direction (`send_recv` default/interp = **1.456× worse**), so this is a repeatable
baseline-JIT message-passing cost, not a one-off. It is listed in the fresh bottleneck ranking (§7 of
`../RESULTS.md`).

> **Do not read this table as a system-ERTS comparison.** These are two *Rust* tiers. The Rust-vs-system
> ratios are in §5. The M9 record's headline "0.71×–0.84× of the system ERTS" was this tier effect
> misattributed; the m10 cross-runtime figures above are the corrected attribution.

## 7. Sys-only external targets (`m10-bench-external.json`, `--full`)

| workload | status | headline | sys peak RSS |
|---|---|---|---|
| `erlc_compiler` | `ok` | wall 6 785.46 ms; 8.842 files/s (60 produced over 46 503 LOC) | 135 405 568 B |
| `elixir_compiler` | `ok` | wall 47 806.41 ms; 4.665 files/s (105 attempted / 223 produced); compiler startup 262.05 ms | 208 289 792 B |
| `cowboy` | `ok` | 210 250.49 req/s; p50 0.0636 ms / p99 0.3020 ms (1 051 576 requests, 0 errors); build 39 511.74 ms | 171 356 160 B |
| `khepri` | `ok` | `rebar3 eunit` wall 78 762.66 ms | 363 016 192 B |
| `plb` (`m10-bench-plb.json`) | `ok` | wall 70 841.43 ms; `test_time_ms` p50 554.76 / p99 3 716.01 (n=10); resolved `dotnet` 9.0.318 | 170 311 680 B |
| `emqx` (`m10-bench-emqx.json`) | `skipped(emqx-requires-otp-28)` | no number | — |

**Every one of these rows is executed by its own system-ERTS toolchain** (`erlc`, `elixirc`, the Cowboy
release on the oracle `erl`, `rebar3`'s OTP-28 Erlang, the `.NET` tool for PLB). **None** is executed by
the Rust runtime — they are **reference** figures for the sys-only surfaces, not Rust-runtime results.

## 8. Honesty check

- Rust rows: all `ok` with real metrics; none `unsupported`, none fabricated, no workload crashed.
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`);
  each run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N`, so per-process startup (including the Rust runtime's code-path
  resolution) cancels on both sides.
- The tier-effect table is labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system
  ERTS) and `rust` (`beam_rt` default tier) explicitly.
- `emqx` is the only non-`ok` row, and it carries a verified reason and **no** number.
- All committed artifact paths are normalised (no worktree/`$TMPDIR` absolutes).
