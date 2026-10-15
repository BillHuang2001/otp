# m12 measurement pass — load record & results pointer

Measurement-and-archive record for the **m12** pass (the full cross-runtime run of the six
`Runtimes::Both` workloads under both runtimes, the interpreter-tier control, the sys-only external-target
run, the `foreign` A/B, and the `run`-route latency probes).
The headline cross-runtime pass was **re-taken on a quiet host** (the first pass ran under a sibling
`cargo test`; it is archived as `m12-bench-xrun-contended.json` and its cross-runtime ratios are
load-biased **in favour of `rust`**). The figures in §5/§5.1/§8 are read from the **quiet re-take**.
Facts only: the exact commands, the `/proc/loadavg` observed before/after each run, the commit measured,
and the sys-vs-rust ratio per workload.
The numbers themselves live in the committed artifacts beside this file:

- `m12-bench-xrun.json` — the full cross-runtime `Report` (schema_version 1, profile `full`, **12/12 rows
  `ok`**), the **quiet re-take** (1-min load `5.77 → 2.09`, wall 1 764 s). This is the report the
  cross-runtime ratios below are read from.
- `m12-bench-xrun-contended.json` — the **superseded first pass** of the same command (wall 1 795 s, load
  `65.28 → 6.93`), kept as the honest cross-check: it ran while a sibling `cargo test` loaded the host, so
  its `sys` rows and its `rust` rows were measured in different conditions.
- `m12-bench-xrun-interp.json` — the same six workloads, `--runtime rust` only, with
  `BEAM_RT_TIER=interpreter` pinned (the **Rust-vs-Rust tier-effect** control — it contains **no** `sys`
  rows and is NOT a system-ERTS comparison). Profile `quick`.
- `m12-bench-xrun-quick.json` — the **matched-form** control: the identical six workloads, `--runtime
  rust`, the **default** (baseline-JIT) tier, profile `quick` — the *same form* as the interpreter
  control, so the tier table is read off two `--quick` runs.
- `m12-compare-cross-runtime.txt` — `bench compare` for the cross-runtime report (sys vs rust inside one file).
- `m12-compare-tier-effect.txt` — `bench compare m12-bench-xrun-interp.json m12-bench-xrun.json`
  (Rust interpreter `--quick` → Rust default `--full`; **form mismatch**; the `b` side is the quiet re-take).
- `m12-compare-tier-effect-quick.txt` — the **matched-form** `--quick` ↔ `--quick` tier control (§6).
- `m12-compare-vs-m11-xrun.txt` / `m12-compare-vs-m11-external.txt` — `bench compare` against the m11 record.
- `m12-bench-external.json` — the sys-only external targets in one `--full` report
  (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`; **4/4 `ok`**).
- `m12-bench-plb.json` — `ok` (the version-aware tool resolution picks `dotnet` 9.0.318).
- `m12-bench-emqx.json` — `skipped(emqx-requires-otp-28)`.
- `m12-foreign-ab-head.json` / `m12-foreign-ab-m10.json` — the matched-`N` `foreign` A/B (b54/b55 shape);
  the narrative is `m12-foreign-regression.md`.
- `m12-run-route-latency.md` — the **OPEN** `run`-route boot / blocked-round latency record.

> **Commit measured.** The driver **and** `beam_rt` were both built from the worktree tree
> **`ee3e426db0`** (`--release -p beam_rt -p bench`) immediately before the pass; the m12 measurement
> artifacts were committed as **`bdadd02333`**. The m10-baseline binary used for the A/B is `beam_rt` from
> **`b4593818b3`**. A sibling agent may rebuild `beam_rt`/`beam_engine` concurrently, so re-record the
> commit if the figures are re-run.

> **What is new at m12 vs m11 (the deliverable of this milestone).** (1) The Θ(N³) `foreign` per-slice
> off-heap registration regression is **FIXED** (commits `07a5720639` + `7d84b12b44`) — the corrected
> `foreign` rust/sys band is **48.0×–76.9×** (m11: 60.2×–367.1×; the load-biased first m12 pass read
> 38.9×–66.9×); see `m12-foreign-regression.md`.
> (2) The `run` action now drives the full `bootstrap/bin/start` boot script (~2.1 s fixed), so **boot
> completes** — `beam_rt -boot bootstrap/bin/start` **exits 0** (m11 recorded exit 3 / "directive 18 of 22")
> — but that same change leaves an **OPEN** per-blocked-round ~25.5 ms scheduler cost and a Θ(atom-table)
> ETF-decode inflation on the `run` route; see `m12-run-route-latency.md`. (3) `khepri` and `plb` now
> **measure** (`ok`); `emqx` still skips honestly.

> **Host-load caveat (read this first).** The benchmark host is a shared, usually-oversubscribed 14-core
> machine. The m12 cross-runtime pass was **re-taken on a quiet host**: the committed
> `m12-bench-xrun.json` started at 1-min `/proc/loadavg` **5.77** and the load fell to **2.09** during
> the run (wall 1 764 s). The **superseded first pass** (archived as `m12-bench-xrun-contended.json`)
> started at **65.28** and fell to **6.93** — it ran while a sibling `cargo test` loaded the host, so
> every **absolute latency/wall/throughput** figure in it is **inflated** by CPU contention.
> **A runtime-major report is additionally biased by drifting load:** the runner emits all `sys` rows
> first, then all `rust` rows, so when the load fell from 65.28 to 6.93 the later (`rust`) side ran under
> a *lighter* load than the `sys` side — i.e. the contended pass' cross-runtime ratios were pushed **in
> favour of `rust`**. That is why the headline pass was re-taken: in the quiet pass both windows are light
> (`sys` at ~5.8, `rust` at ~2.1). Even so, absolute values are load-sensitive and must not be read as
> machine-speed constants; the load-robust signals are the **per-op / per-element / per-call marginals**
> (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counts**
> (`*_ops`, `*_elements`, `*_calls`), because the per-runtime loop count `N` is fixed per case.

## 1. Environment

- Commit measured: `ee3e426db0` (runtime+driver tree); artifacts committed as `bdadd02333`.
- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1, `erlang:system_info(version)` = `17.1`) at
  `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in the report.
- Host: NixOS 26.11 (Zokor), Linux 7.2.9 (`x86_64`), Intel(R) Core(TM) Ultra 7 356H, `nproc` = 14,
  `MemTotal` = 65 317 432 kB (~62.3 GiB).
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.0`; `clang` 21.1.8 on `PATH`.
- `erlc` = the oracle's `erlc`; `elixirc`/`mix` **1.20.4** ("compiled with Erlang/OTP 29").
- `dotnet` **9.0.318** is on `PATH` (`/run/current-system/sw/bin/dotnet`); the store also holds two
  8.0.425 SDKs, which the version-aware resolver rejects for `plb` (min `9`).
- `rebar3` is **not** on `PATH` — resolved from the Nix store by the driver's store scan
  (`rebar3-3.27.1`), so `khepri` runs with no `PATH` prefix.
- `$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` are **unset**; `$ERTS_ORACLE` / `$BEAM_RT` / `$BEAM_RT_TIER`
  are **unset** for every run except the interpreter control (which exports `BEAM_RT_TIER=interpreter`)
  and the `foreign` A/B (which exports `BEAM_RT`).
- `$HOME` is read-only; `/usr/bin/time` is **absent** (the driver's own timers and `/proc/<pid>`
  `VmHWM` polling are used).
- `erts_rust/target/release/bench list --json`: **all 12 workloads `probe_ready: true`, 0 skip reasons.**

## 2. Exact commands

```sh
# build (once; the wrapper supplies a writable CARGO_HOME — $HOME is read-only)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# probe status
erts_rust/target/release/bench list --json

# A. FULL cross-runtime run — the six Runtimes::Both workloads, --full, both runtimes.
#    RE-TAKEN on a quiet host: wall 1764 s, 1-min load 5.77 -> 2.09. The first, contended pass
#    (wall 1795 s at load 65.28 -> 6.93) is archived as results/m12-bench-xrun-contended.json.
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m12-bench-xrun.json --verbose

# B. tier controls — same six workloads, rust only (wall 391 s interpreter / 488 s default)
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m12-bench-xrun-interp.json
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m12-bench-xrun-quick.json

# C. FULL sys-only external targets (one report; wall 184 s)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m12-bench-external.json --verbose

# D. PLB (quick, wall 95 s)   E. EMQX (quick, skips immediately, wall 1 s)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m12-bench-plb.json  --verbose
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m12-bench-emqx.json --verbose

# F. Step-5 run-route latency probes (direct beam_rt CLI; not a report — m12-run-route-latency.md)
#    HEAD vs the m10 binary vs HEAD-with-empty-boot, send_recv / send_recv_plain / etf / ets / -boot
# G. Step-4 foreign A/B + growth ladder (direct beam_rt run on bench_foreign_driver:enc/1,
#    interleaved 5 passes at N=5000/10000/25000/60000, bracketed by two N=0) and the b54/b55-shape
#    driver A/B pair (m12-foreign-ab-{head,m10}.json) — m12-foreign-regression.md

# H. compare artifacts (the tool diffs the mean) — regenerated after the quiet re-take, so the
#    tier-effect diff's `b` side and the vs-m11 xrun diff both use the quiet m12-bench-xrun.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-xrun.json > .../m12-compare-cross-runtime.txt
erts_rust/target/release/bench compare .../m12-bench-xrun-interp.json .../m12-bench-xrun.json            > .../m12-compare-tier-effect.txt
erts_rust/target/release/bench compare .../m12-bench-xrun-interp.json .../m12-bench-xrun-quick.json      > .../m12-compare-tier-effect-quick.txt
erts_rust/target/release/bench compare .../m11-bench-xrun.json     .../m12-bench-xrun.json             > .../m12-compare-vs-m11-xrun.txt
erts_rust/target/release/bench compare .../m11-bench-external.json .../m12-bench-external.json         > .../m12-compare-vs-m11-external.txt
```

Workloads were run **serially** — this driver was the only benchmark process, one run at a time (A → B →
C → D → E → F → G). The artifacts were written to `results/` with only the two mechanical path
normalisations (the run-host worktree prefix in `runtimes[].program` → `erts_rust/target/release/beam_rt`;
a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

## 3. `/proc/loadavg` record (1-min value first, in the order the passes were run)

| pass | before (1-min) | after (1-min) | wall | artifact |
|---|---|---|---|---|
| **A. FULL cross-runtime pass — QUIET RE-TAKE** | `5.77` | `2.09` | 1764 s (29m24s) | `m12-bench-xrun.json` |
| **A′. FULL cross-runtime pass — first (contended) pass, RE-RUN below** | `65.28` | `6.93` | 1795 s (29m55s) | `m12-bench-xrun-contended.json` |
| **B. tier controls** (interpreter `--quick`, then matched default `--quick`) | `3.91` | `11.69` | 391 s + 488 s | `m12-bench-xrun-interp.json`, `m12-bench-xrun-quick.json` |
| **C. FULL external-targets pass** | `10.91` | `9.34` | 184 s | `m12-bench-external.json` |
| **D. PLB quick pass** | `9.34` | `7.85` | 95 s | `m12-bench-plb.json` |
| **E. EMQX quick pass** (skips immediately) | `7.85` | `7.54` | 1 s | `m12-bench-emqx.json` |
| **F. run-route latency probes** (§9; not a report) | `10.26` | `8.86` | ≈174 s | `m12-run-route-latency.md` |
| **G. `foreign` A/B + growth ladder** (§3.4; not a report) | `6.72` | `5.73` | ≈249 s | `m12-foreign-regression.md`, `m12-foreign-ab-{head,m10}.json` |

The archived artifacts' own `generated_at_unix_ms` (UTC) are: **`m12-bench-xrun.json` 15:46:06** (the
quiet re-take), `m12-bench-xrun-contended.json` **14:29:57** (the superseded first pass),
`m12-bench-xrun-interp.json` **14:38:42**, `m12-bench-xrun-quick.json` **14:46:50**,
`m12-bench-external.json` **14:49:58**, `m12-bench-plb.json` **14:51:33**, `m12-bench-emqx.json`
**14:51:34**. Session span (step 1 start 14:00:02 → last probe 15:03:33, then the quiet re-take
15:16:42 → 15:46:06): **2026-10-15 14:00–15:46 UTC** (= 2026-10-15 22:00–23:46 +0800). The 1-min load
across the session ranged **2.09 → 65.28**. The step-B batch's two sub-runs share the window
14:30Z → 14:46:50Z (the default-tier quick run's end logged `loadavg=11.69 6.42 12.36`); the quiet
cross-runtime re-take (A) ran alone in the 15:16–15:46Z window.

**Why the first pass was re-taken.** The pass A′ (`m12-bench-xrun-contended.json`) began while a sibling
agent's `cargo test` was saturating the host (1-min load **65.28**), and the load had fallen to **6.93** by
the time it finished. Because the runner is runtime-major (every `sys` row, then every `rust` row), that
means its `sys` side was measured under far heavier contention than its `rust` side — biasing the
cross-runtime ratios **toward `rust`** (e.g. `baseline startup_ms` read 19.05× then vs **18.44×** now;
`foreign` band 38.9×–66.9× then vs **48.0×–76.9×** now; `slice` 13.7×–17.5× then vs **15.7×–18.7×** now).
The host was quiet afterwards, so pass A was re-taken alone (`5.77 → 2.09`) and is now the committed
`m12-bench-xrun.json` and the basis of §5/§5.1/§8. The contended copy is retained as the honest
cross-check; **no metric in it was altered**.

## 4. Per-workload status (full cross-runtime pass, `m12-bench-xrun.json`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust | cases |
|---|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` | — |
| `slice`      | compute   | `ok` | `ok` | 9/9 ok on each side |
| `foreign`    | compute   | `ok` | `ok` | 4/4 ok on each side |
| `list_walk`  | compute   | `ok` | `ok` | 1/1 ok on each side |
| `copy_bound` | messaging | `ok` | `ok` | **sys 4/4 ok; rust 3 ok, 0 unsupported (of 4)** — `send_recv_plain` hit the 60 s `RUN_DEADLINE` (§9, `m12-run-route-latency.md`) |
| `gc_bound`   | gc        | `ok` | `ok` | 1/1 ok on each side |

No rust row is fabricated: every rust row carries real `Summary`/scalar metrics (no `null`-only rows, no
`unsupported` classification, no crash/exit-3). Every copy/churn/walk case validated its own round-trip
with a bound-variable pattern on both sides and printed the deterministic constant (recorded in the
report's `observed stdout:` notes), so the two runtimes executed identical work.

The tier controls are **6 rust rows each, all `ok`** (no `sys` rows by construction). The full external
pass (`m12-bench-external.json`) is **4 rows, all `ok`**. `m12-bench-plb.json` is **1 `ok`** row;
`m12-bench-emqx.json` is **1 `skipped`** row (no metric).

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`rust/sys` > 1 means the Rust side costs **more** per unit (its ns value is higher). The marginals
(`*_ns_*`) are the load-robust cross-runtime signal; the wall/startup rows are inflated by the load and
are shown for completeness. These are read from the **quiet** `m12-bench-xrun.json` (1-min load
`5.77 → 2.09`); the superseded contended pass is in `m12-bench-xrun-contended.json`. **Both sides of
every ratio are named** — `sys` is the system ERTS (OTP 29.1.1 / erts-17.1), `rust` is `beam_rt` at its
**default** (baseline-JIT) tier.

| workload | headline cross-runtime metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 109.1118 ms | 2 011.9543 ms | **18.44×** (rust dearer — the fixed boot; §9) |
| `baseline`   | `vm_memory_total_bytes` | 36 221 280 B | 15 892 346 B | **0.439×** (rust 2.28× lower) |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 122.24–132.59 ms | 2 076.98–2 443.59 ms | **15.7×–18.7×** (rust dearer — whole-process wall, boot-dominated) |
| `foreign`    | `case_<name>_ns_per_call` | 242.64–484.94 ns | 15 675.95–26 890.93 ns | **48.0×–76.9×** (rust dearer; the Θ(N³) fix narrowed it from m11's 60.2×–367.1×) |
| `list_walk`  | `case_list_walk_ns_per_element` | 3.3433 ns | **absent** (marginal below timer resolution) | — (no matched pair; §3.5) |
| `copy_bound` | `case_<name>_ns_per_op` | 259.57–1 273.94 ns | 2 876.40–25 447 011.27 ns | **11.08×–19 975.1×** (rust dearer; `send_recv` is the boot/blocked-round defect, §9) |
| `gc_bound`   | `case_churn_ns_per_op` | 4 448.9426 ns | 108 250.5757 ns | **24.33×** (rust dearer) |

### 5.1 Per-case detail

`baseline` (whole-process cost):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `startup_ms` (mean, n=50) | 109.1118 | 2 011.9543 | 18.44× |
| `startup_ms` (p50 / p99) | 109.3437 / 122.4731 | 1 997.9086 / 2 279.0746 | — |
| `compute_loop_ms` (mean, n=5) | 265.6280 | 2 059.9596 | 7.76× |
| `compute_ops_per_sec` | 3 764 663.72 | 485 446.41 | rust 0.129× |
| `vm_memory_total_bytes` | 36 221 280 | 15 892 346 | rust 2.28× lower |
| `peak_memory_bytes` (RSS) | 137 490 432 (131.1 MiB) | 408 170 496 (389.3 MiB) | rust 2.97× higher |

`slice` (one call per process: `case_<name>_wall_ms` mean; peak RSS p50):

| case | sys wall_ms | rust wall_ms | rust/sys | sys RSS (MiB) | rust RSS (MiB) |
|---|---|---|---|---|---|
| `add` | 132.5896 | 2 076.9769 | 15.66× | 66.48 | 376.25 |
| `fib` | 127.3980 | 2 087.0114 | 16.38× | 66.25 | 376.25 |
| `pick` | 123.1796 | 2 101.8715 | 17.06× | 66.30 | 376.26 |
| `calendar_last_day` | 122.8615 | 2 136.5924 | 17.39× | 66.38 | 376.31 |
| `queue_from_size` | 122.2377 | 2 255.2689 | 18.45× | 66.74 | 376.34 |
| `gb_trees_size` | 130.4145 | 2 443.5910 | 18.74× | 66.54 | 376.31 |
| `base64_encode_size` | 122.9839 | 2 233.0740 | 18.16× | 66.27 | 377.33 |
| `ordsets_from_size` | 125.7476 | 2 095.0575 | 16.66× | 66.48 | 376.27 |
| `string_str_length` | 124.5368 | 2 225.2449 | 17.87× | 66.29 | 376.71 |

`slice` whole-workload `peak_memory_bytes`: sys 71 991 296 (68.65 MiB) → rust 395 755 520 (377.42 MiB).
**Both walls now include the ~2.1 s `run`-route boot on the rust side (§9), so the ratio is boot-dominated,
not an execution-throughput measure; the rust RSS is the boot's resident preloaded-module set.**

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys | sys startup p50 (ms) | rust startup p50 (ms) | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|---|
| `bstr_to_lower` | 349.6085 | 26 890.9337 | 76.92× | 229.40 | 2 181.86 | 66.19 | 438.48 |
| `bstr_capitalize_token` | 484.9422 | 23 283.5499 | 48.01× | 236.07 | 2 092.83 | 66.19 | 441.29 |
| `emqx_base62_encode` | 242.6446 | 15 675.9526 | 64.60× | 124.92 | 2 089.91 | 66.46 | 427.54 |
| `emqx_base62_decode` | 327.3718 | 16 851.8374 | 51.48× | 235.03 | 2 076.56 | 66.25 | 427.94 |

`list_walk` (BIF-free recursion — the per-element dispatch signal):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 3.3433 | **absent** (`marginal below timer resolution`) | — |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 | (per-runtime `N`, not a ratio) |
| `case_list_walk_peak_rss_bytes` p50 | 66.26 MiB | 376.52 MiB | boot-inflated |

`copy_bound` (term-copy boundary; the `send_recv_plain` rust case **timed out** — no rust metric):

| case | sys ns/op | rust ns/op | rust/sys | ops sys / rust | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | 779.8111 | 40 882.4466 | **52.43×** | 1 500 000 / 5 000 | 65.74 | 382.65 |
| `ets_copy` | 259.5729 | 2 876.4047 | **11.08×** | 2 000 000 / 100 000 | 66.27 | 449.86 |
| `send_recv` | 1 273.9382 | 25 447 011.2735 | **19 975.1×** | 500 000 / 2 000 | 66.63 | 384.29 |
| `send_recv_plain` | 696.0458 | **— (failed: 60 s deadline)** | — | 1 000 000 / — | 66.21 | — |

`copy_bound` whole-workload `peak_memory_bytes`: sys 72 089 600 (68.75 MiB) → rust 471 982 080 (450.12 MiB).
`etf_roundtrip`'s rust cost is the **Θ(atom-table) `binary_to_term`** boot effect and `send_recv`'s is the
**~25.5 ms blocked-round** boot effect (§9, `m12-run-route-latency.md`) — both OPEN, both `beam_rt`.

`gc_bound`:

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_churn_ns_per_op` | 4 448.9426 | 108 250.5757 | 24.33× |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 | (per-runtime `N`) |
| `case_churn_peak_rss_bytes` p50 | 66.48 MiB | 376.52 MiB | boot-inflated |

Deterministic counters (contention-immune), unchanged per case from m11: `case_*_ops` = 1 500 000 /
2 000 000 / 500 000 / 1 000 000 (sys copy_bound), 5 000 / 100 000 / 2 000 (rust copy_bound);
`case_*_calls` = 1 500 000 / 1 500 000 / 3 000 000 / 3 000 000 (sys foreign), 20 000 / 15 000 / 25 000 /
15 000 (rust foreign); `case_list_walk_elements` = 1e8 sys / 1e6 rust; `case_churn_ops` = 200 000 sys /
2 000 rust. All observed stdout matched the `N × 1` / `N × 500500` constant on both sides.

## 6. Tier effect (Rust vs Rust — **not** a system-ERTS comparison)

`rust` default tier / `rust` interpreter tier, read from the **matched-form `--quick`** pair
(`m12-compare-tier-effect-quick.txt`: `a` = interpreter, `b` = default). `> 1` means the default
(baseline-JIT) tier is **cheaper**; `< 1` means the interpreter is cheaper.

| workload | metric | interpreter | default (baseline JIT) | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 1 959.5795 | 2 086.4559 | 0.939× (default marginally dearer) |
| `baseline` | `compute_loop_ms` | 1 991.3159 | 2 137.2065 | 0.932× (noise; native `lists:seq/sum` floor) |
| `baseline` | `vm_memory_total_bytes` | 15 895 354 | 15 892 346 | 1.000× (parity) |
| `list_walk` | `ns_per_element` | 235.0513 | **absent** ("marginal below timer resolution") | — (see note) |
| `gc_bound` | `churn` ns/op | 308 835.7985 | **148 964.3747** | **2.07×** (default cheaper) |
| `copy_bound` | `etf_roundtrip` ns/op | 44 170.6459 | 44 687.5942 | 0.988× (parity) |
| `copy_bound` | `ets_copy` ns/op | 2 996.3663 | 3 014.0485 | 0.994× (parity) |
| `copy_bound` | **`send_recv` ns/op** | 25 516 698.9983 | **25 537 116.8888** | **0.999× — parity** |
| `copy_bound` | **`send_recv_plain` ns/op** | **timed out (60 s)** | **timed out (60 s)** | — (no metric either tier) |
| `foreign` | 4 cases ns/call | 21 458–37 786 | 15 449–21 014 | 1.03×–1.80× (default cheaper) |
| `slice` | 9 cases wall_ms | 2 021–2 104 | 2 101–2 165 | 0.95×–1.06× (boot-dominated; no stable tier effect) |

Three facts to carry forward for the tier table:

- **`list_walk`'s `ns_per_element` is absent in the default-tier `--quick` run** — the workload emitted
  the honest note `case list_walk: marginal below timer resolution; no ns_per_element metric` (the loop
  marginal is swamped by the ~2.1 s boot's jitter at only 3 samples). The interpreter quick run *did*
  measure **235.05 ns/element**, but the quiet `--full` cross-runtime default run published **no**
  `ns_per_element` either (its rust marginal did not resolve, §5) — so **no matched default-tier pair
  exists at m12** and no `list_walk` tier conclusion is drawn.
- **`send_recv_plain` timed out in both tier runs** (the same 60 s `RUN_DEADLINE`, §9), so there is no
  tier pair for it.
- **The m11 interpreter pathology is GONE.** At m11 the interpreter tier was super-linear on the `foreign`
  cross-module cases (up to **4 178 832 ns/call**, a `--full` pass ≈50 min); at m12 the interpreter
  `foreign` rows are **21 458–37 786 ns/call** (1.03×–1.80× the default) and the whole interpreter
  `--quick` control completed in **391 s**. That is consistent with the Θ(N³) registration regression
  having been **fixed** (`m12-foreign-regression.md`) — the m11 interpreter pathology was the *same*
  shared `beam_rt`/`beam_process` defect, not an interpreter-tier defect.

**The requested `copy_bound` rows, with direction:** `send_recv` **25 516 699 → 25 537 117 ns/op
(Δ = +20 418, +0.08 %)** — **parity** (and now dominated by the 25.5 ms blocked-round defect, identical
in both tiers). `send_recv_plain` has **no metric in either tier** (both timed out).

The as-specified artifact `m12-compare-tier-effect.txt` (interpreter `--quick` vs the **quiet** default
`--full` `m12-bench-xrun.json`; its `b` side is the re-taken report) gives the same directions where both
tiers have a value: `send_recv` 25 516 699 → 25 447 011 (**−0.27 %**), `gc_bound` −64.95 %, `foreign`
−23.2 % to −45.7 %, `copy_bound etf_roundtrip` −7.44 %, `ets_copy` −4.00 %; `list_walk`
`metric-only-in-a`, and the `sys` rows `only-in-b` by construction.

## 7. Sys-only external targets (`m12-bench-external.json`, `--full`)

| workload | status | headline | sys peak RSS |
|---|---|---|---|
| `erlc_compiler` | `ok` | wall 6 842.18 ms; 8.769 files/s (60 produced over 46 503 LOC) | 134 492 160 B |
| `elixir_compiler` | `ok` | wall 50 304.34 ms; 4.433 files/s (105 attempted / 223 produced); compiler startup 297.64 ms | 211 509 248 B |
| `cowboy` | `ok` | 165 489.21 req/s; p50 0.0779 ms / p99 0.7424 ms (827 888 requests, 0 errors); build 42 527.11 ms | 166 875 136 B |
| `khepri` | `ok` | `rebar3 eunit` wall 77 564.34 ms | 374 722 560 B |
| `plb` (`m12-bench-plb.json`) | `ok` | wall 70 648.46 ms; `test_time_ms` p50 595.13 / p99 3 565.99 (n=10); resolved `dotnet` 9.0.318 | 171 573 248 B |
| `emqx` (`m12-bench-emqx.json`) | `skipped(emqx-requires-otp-28)` | no number | — |

**Every one of these rows is executed by its own system-ERTS toolchain** (`erlc`, `elixirc`, the Cowboy
release on the oracle `erl`, `rebar3`'s OTP-28 Erlang, the `.NET` tool for PLB). **None** is executed by
the Rust runtime — they are **reference** figures for the sys-only surfaces, not Rust-runtime results.

## 8. m12 vs m11 (from `m12-compare-vs-m11-xrun.txt` / `m12-compare-vs-m11-external.txt`)

The tool diffs the **mean**; `a` = m11, `b` = m12. Rust-default-tier rows first (the interesting ones):

| (workload, metric) | m11 | m12 | Δ |
|---|---|---|---|
| `foreign` `emqx_base62_encode` ns/call **rust** | 77 167.3229 | **15 675.9526** | **−79.69 %** (the Θ(N³) fix) |
| `foreign` `bstr_capitalize_token` ns/call **rust** | 28 757.0043 | **23 283.5499** | −19.03 % |
| `foreign` `emqx_base62_decode` ns/call **rust** | 19 306.3883 | **16 851.8374** | −12.71 % |
| `foreign` `bstr_to_lower` ns/call **rust** | 23 751.0676 | **26 890.9337** | +13.22 % |
| `baseline` `startup_ms` **rust** | 15.2122 | **2 011.9543** | **+13 125.97 %** (the fixed boot; §9) |
| `baseline` `compute_loop_ms` **rust** | 58.6524 | **2 059.9596** | +3 412.15 % (boot) |
| `baseline` `vm_memory_total_bytes` **rust** | 805 458 | **15 892 346** | +1 873.08 % (boot's resident set) |
| `copy_bound` `send_recv` ns/op **rust** | 21 522.2389 | **25 447 011.2735** | **+118 135.89 %** (the blocked-round defect; §9) |
| `copy_bound` `etf_roundtrip` ns/op **rust** | 7 033.3943 | **40 882.4466** | +481.26 % (Θ(atom-table) decode; §9) |
| `copy_bound` `ets_copy` ns/op **rust** | 2 435.0664 | **2 876.4047** | +18.12 % |
| `copy_bound` `send_recv_plain` (rust rows) | present | **failed (60 s deadline)** | rust metrics dropped (`metric-only-in-a`) |
| `gc_bound` `churn` ns/op **rust** | 128 524.4542 | **108 250.5757** | **−15.77 %** (rust marginal is load-sensitive) |
| `list_walk` ns/element **rust** | 45.9731 | **absent** (`metric-only-in-a`) | the rust marginal did not resolve (§5) |
| `slice` rust walls | 15.35–37.85 ms | **2 076.98–2 443.59 ms** | the fixed boot (one process per case) |

`sys`-side rows moved only with the host (both passes light: `baseline` `startup_ms` **−3.28 %**,
`foreign` ns/call **+1.34 % to +49.21 %**, `copy_bound` ns/op **−12.35 % to +19.88 %**, `gc_bound` churn
+4.19 %, `list_walk` ns/element +38.19 %, `slice` walls +0.84 % to +9.89 %; `compute_loop_ms` +50.77 %
is the row's own n=5 noise) — no `sys` move is remotely the size of the `rust` moves, which is the
cross-check that the rust moves are not oracle drift.

External targets (m11 → m12): `erlc_compiler` wall 7 905.65 → 6 842.18 ms (**−13.45 %**, files/s +15.54 %);
`elixir_compiler` wall 47 667.62 → 50 304.34 ms (+5.53 %, files/s −5.24 %); `cowboy` `requests_per_sec`
190 626.72 → 165 489.21 (**−13.19 %**) and mean latency +24.34 %; `khepri` wall 83 402.90 → 77 564.34 ms
(−7.00 %). `plb` (read manually from `m11-bench-plb.json` → `m12-bench-plb.json`, no tool diff archived):
wall 68 318.69 → 70 648.46 ms (**+3.41 %**). All are on the shared host and none is a like-for-like
quiet-host comparison.

**Three real, code-attributable m12 moves** (owners **outside this node**, `beam_rt`):
1. **`foreign` Θ(N³) fix** — the four rust `ns_per_call` rows fell (encode −79.69 %), and the corrected
   cross-runtime band is **48.0×–76.9×** (`m12-foreign-regression.md`).
2. **The `run`-route boot (`85ee33b2f8`)** — +~2.1 s fixed startup on every `run`/`-eval`/`-boot` action
   (`baseline`/`slice` walls, rust RSS ≈376–450 MiB), the Θ(atom-table) `binary_to_term` inflation
   (`etf_roundtrip` +481.26 %), and the ~25.5 ms-per-blocked-round scheduler cost that pushes `send_recv`
   to 25.45 ms/op and `send_recv_plain` past its 60 s deadline — **all OPEN** (§9,
   `m12-run-route-latency.md`).
3. **Boot now completes** — `beam_rt -boot bootstrap/bin/start` **exits 0** (m11: exit 3, "directive 18 of
   22"). The m11 boot-status claim is obsolete (§9).

## 9. Runtime boot status & the OPEN `run`-route defect at this commit

A factual probe, not a workload — full narrative in `results/m12-run-route-latency.md` (**OPEN**):

```
$ erts_rust/target/release/beam_rt -boot bootstrap/bin/start
exit_code=0        wall ≈2.07–2.09 s
Erlang/OTP 30 [erts-17.1] [source] [64-bit]
```

- **Boot completes** (m11 recorded exit 3 on `opcode: run: process blocked (no message available)` at
  directive 18 of 22) — the m11 boot-status fact is **obsolete**.
- **Fixed `run`-route startup ≈2.07–2.13 s** (HEAD) vs 17–19 ms (m10) vs 18–19 ms (HEAD with a synthetic
  empty boot script); `-version` still costs 2 ms.
- **~25.5 ms per blocked round** with ~2 % CPU (`send_recv` N=2 000: wall 53 141 ms, user CPU 1.249 s;
  ≈220× m10) — the booted kernel's `os:set_signal/2` self-pipe keeps `PortRegistry::has_nothing_to_poll()`
  false, so every blocked round sleeps the full `PORT_POLL_SLICE` = 25 ms (`runtime/scheduler.rs`, `port.rs`).
- **Θ(atom-table) ETF decode** — boot grows the atom table ~70 → ~4 100, so `binary_to_term`'s
  `prefix_matches` validation is Θ(table) (the `etf_roundtrip` inflation).
- Root cause owned by `beam_rt` (commit `85ee33b2f8`); already documented at
  `erts_rust/crates/beam_rt/src/runtime/CONTEXT.md:150` and `results/b55-foreign-regression-fix.md`.
  **Not** attributable to any target project.

## 10. Honesty check

- Rust rows: all `ok` with real metrics; the single exception is `copy_bound`'s `send_recv_plain` rust
  case, which is recorded as **failed (60 s deadline)** — no metric fabricated, and it does **not** change
  the row status (the row carries `3 ok, 0 unsupported (of 4)`).
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`); each
  run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N`, so per-process startup (including the ~2.1 s rust boot) cancels on
  both sides. The `send_recv_plain` rust case cannot produce one, so it is reported as a failure, not a
  number.
- The tier tables are labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS) and
  `rust` (`beam_rt` default tier) explicitly. The two tier runs are both `--quick` (matched form); the
  as-specified `m12-compare-tier-effect.txt` is accompanied by `m12-compare-tier-effect-quick.txt`.
- **The loop workloads' rust peak RSS (≈377–450 MiB) is boot-dominated and NOT like-for-like**, and the
  `foreign`/`copy_bound` per-op rows are additionally `N`-dependent on the rust side — neither is a clean
  memory context (`slice` remains the per-process metric in principle, but at m12 its rust RSS is also the
  boot's resident set).
- `emqx` is the only non-`ok` row, and it carries a verified reason and **no** number.
- All committed artifact paths are normalised (no worktree/`$TMPDIR` absolutes).
