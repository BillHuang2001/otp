# m11 measurement pass — load record & results pointer

Measurement-and-archive record for the **m11** pass (the full cross-runtime run of the six
`Runtimes::Both` workloads under both runtimes, the interpreter-tier control, and the sys-only
external-target run).
Facts only: the exact commands, the `/proc/loadavg` observed before/after each run, the commit measured,
and the sys-vs-rust ratio per workload.
The numbers themselves live in the committed artifacts beside this file:

- `m11-bench-xrun.json` — the full cross-runtime `Report` (schema_version 1, profile `full`, **12/12 rows
  `ok`**). This is the report the cross-runtime ratios below are read from.
- `m11-bench-xrun-interp.json` — the same six workloads, `--runtime rust` only, with
  `BEAM_RT_TIER=interpreter` pinned (the **Rust-vs-Rust tier-effect** control — it contains **no** `sys`
  rows and is NOT a system-ERTS comparison). **Profile `quick`** (see §3 — the interpreter tier is
  pathologically slow on the cross-module `foreign` cases at this commit, so the `--full` form was not
  affordable; this is the documented fallback).
- `m11-bench-xrun-quick.json` — **supplementary matched-form control**: the identical six workloads,
  `--runtime rust`, the **default** (baseline-JIT) tier, profile `quick` — i.e. the *same form* as
  `m11-bench-xrun-interp.json`. It exists so the tier table can be read off two `--quick` runs instead of
  pairing a `--quick` run against the `--full` `m11-bench-xrun.json`; it is **not** a system-ERTS
  comparison either.
- `m11-compare-cross-runtime.txt` — `bench compare` for the cross-runtime report (sys vs rust inside one file).
- `m11-compare-tier-effect.txt` — `bench compare m11-bench-xrun-interp.json m11-bench-xrun.json`
  (Rust interpreter → Rust default; `a` = interpreter). **Form mismatch:** `a` is `--quick`, `b` is
  `--full`.
- `m11-compare-tier-effect-quick.txt` — `bench compare m11-bench-xrun-interp.json m11-bench-xrun-quick.json`
  (the **matched-form** `--quick` ↔ `--quick` tier control; this is the pair §6's tier table is read from).
- `m11-compare-vs-m10-xrun.txt` / `m11-compare-vs-m10-external.txt` — `bench compare` against the m10 record.
- `m11-bench-external.json` — the sys-only external targets in one `--full` report
  (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`; **4/4 `ok`**).
- `m11-bench-plb.json` — `ok` (the version-aware tool resolution picks `dotnet` 9.0.318).
- `m11-bench-emqx.json` — `skipped(emqx-requires-otp-28)`.

> **Commit measured.** Everything below was measured at **`ab6f1b5e58835cbf5fcb75257679532c46090a74`**
> (`ab6f1b5e58`, HEAD: "bench(copy_bound): add send_recv_plain delivery-only attribution case"). The
> driver **and** `beam_rt` were both built from this tree (`--release -p beam_rt -p bench`) immediately
> before the pass. A sibling agent may rebuild `beam_rt`/`beam_engine` concurrently, so re-record the
> commit if the figures are re-run.
>
> **What is new at m11 vs m10:** the `copy_bound` workload gained a fourth case, **`send_recv_plain`**
> (the identical two-process shape as `send_recv` but with the payload replied *directly*, i.e. with the
> `term_to_binary`/`binary_to_term` leg removed from the receiver), so `copy_bound` is now
> `etf_roundtrip` + `ets_copy` + `send_recv` + `send_recv_plain`.

> **Host-load caveat (read this first).** The benchmark host is a shared, usually-oversubscribed
> 14-core machine. During this session the 1-minute `/proc/loadavg` ran **from 1.12 to 7.18** (see the
> record below). Every **absolute latency/wall/throughput** figure in the artifacts is therefore
> **inflated** by CPU contention when the load was high and must not be read as a machine-speed
> constant. The load-robust signals are the **per-op / per-element / per-call marginals**
> (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counts**
> (`*_ops`, `*_elements`); the per-runtime loop count `N` is fixed per case, so the per-unit figures are
> the comparable ones across the two runtimes.
> **A runtime-major report can be biased by drifting load:** the runner emits all `sys` rows first,
> then all `rust` rows, so if the load drifts during a run the two sides are measured in different
> conditions. The full cross-runtime pass below **fell** from 6.04 to 2.98, i.e. the later (`rust`) side
> ran under a *lighter* load — read the ratios, not the absolute pair. The external pass and the tier
> control were run at ~1.4–2.6 (quiet), which is the better half of the session.

## 1. Environment

- Commit measured: `ab6f1b5e58835cbf5fcb75257679532c46090a74`.
- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1, `erlang:system_info(version)` = `17.1`) at
  `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in the report.
- Host: NixOS 26.11 (Zokor, build `26.11.20261003.a7868a7`), Linux 7.2.9 (`x86_64`), `nproc` = 14,
  `MemTotal` = 65 317 432 kB (~62.3 GiB).
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.0`; `clang` 21.1.8 on `PATH`
  (`/run/current-system/sw/bin/clang`).
- `erlc` = the oracle's `erlc` (`…/erlang-29.1.1/lib/erlang/erts-17.1/bin/erlc`); `elixirc`/`mix`
  **1.20.4** ("compiled with Erlang/OTP 29", `/etc/profiles/per-user/bill/bin/mix`).
- `dotnet` **9.0.318** is on `PATH` (`/run/current-system/sw/bin/dotnet`); the store also holds two
  8.0.425 SDKs, which the version-aware resolver rejects for `plb` (min `9`).
- `rebar3` is **not** on `PATH` — resolved from the Nix store by the driver's store scan
  (`/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1`, newest of 3.27.0/3.27.1).
- `$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` are **unset**; `$ERTS_ORACLE` / `$BEAM_RT` / `$BEAM_RT_TIER`
  are **unset** for every run except the interpreter control (which exports `BEAM_RT_TIER=interpreter`).
- `$HOME` is read-only; `/usr/bin/time` is **absent** (the driver's own timers and `/proc/<pid>`
  `VmHWM` polling are used).
- `erts_rust/target/release/bench list --json`: **all 12 workloads `probe_ready: true`, 0 skip reasons.**

## 2. Exact commands

```sh
# build (once; the wrapper supplies a writable CARGO_HOME — $HOME is read-only)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# probe status
erts_rust/target/release/bench list --json

# A. FULL cross-runtime run — the six Runtimes::Both workloads, --full, both runtimes (wall 2m54s)
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m11-bench-xrun.json --verbose

# C. FULL sys-only external targets (one report; wall 3m11s)
erts_rust/target/release/bench run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/out/m11-bench-external.json --verbose

# D. PLB (quick, wall 1m33s)   E. EMQX (quick, skips immediately, wall 0.7s)
erts_rust/target/release/bench run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/out/m11-bench-plb.json  --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/out/m11-bench-emqx.json --verbose

# B. tier-effect control — the same six workloads, rust only, interpreter pinned (wall 12m27s)
#    PROFILE quick: see the interpreter-tier note below (the --full form is ~4x this at this commit)
BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m11-bench-xrun-interp.json --verbose

# B2. supplementary MATCHED-form default-tier control (quick, same six workloads, wall 17s)
erts_rust/target/release/bench run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m11-bench-xrun-quick.json

# step 5: the runtime's boot status at this commit (factual probe only)
erts_rust/target/release/beam_rt -boot bootstrap/bin/start
```

Workloads were run **serially** — this driver was the only benchmark process, one run at a time
(the A → C → D → E → B → B2 order in §3 is the actual order; a set of short, unarchived interpreter
diagnostics ran between A and C and is listed there too). The artifacts were written to the gitignored
`out/` and then archived into `results/` with only the two mechanical path normalisations (the run-host
worktree prefix in `runtimes[].program` → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix
in `notes` → `$TMPDIR/`). **No metric value was altered** (the archive script asserts no `/home/bill`,
`/tmp`, or `/Source` absolute survives in the archived JSON).

## 3. `/proc/loadavg` record (1-min value first, in the order the passes were run)

| pass | before (1-min) | after (1-min) | wall | artifact |
|---|---|---|---|---|
| **A. FULL cross-runtime pass** | `6.04` | `2.98` | 2m54s (173 952 ms) | `m11-bench-xrun.json` |
| (diagnostics: `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` under the interpreter tier, plus direct `beam_rt` probes; **not archived**) | `3.53` | `2.07` | ~14m total | — |
| **C. FULL external-targets pass** | `1.36` | `2.36` | 3m11s (190 891 ms) | `m11-bench-external.json` |
| **D. PLB quick pass** | `2.36` | `1.97` | 1m33s (92 813 ms) | `m11-bench-plb.json` |
| **E. EMQX quick pass** (skips immediately) | `1.97` | `1.97` | 0.7s (727 ms) | `m11-bench-emqx.json` |
| **B. interpreter-tier control (quick)** | `1.81` | `2.59` | 12m27s (747 110 ms) | `m11-bench-xrun-interp.json` |
| **B2. matched default-tier control (quick)** | `2.38` | `2.16` | 17s (16 669 ms) | `m11-bench-xrun-quick.json` |

The archived artifacts' own `generated_at_unix_ms` (UTC) bookend the session:
`m11-bench-xrun.json` **05:51:43**, `m11-bench-external.json` **06:41:57**, `m11-bench-plb.json`
**06:43:32**, `m11-bench-emqx.json` **06:43:35**, `m11-bench-xrun-interp.json` **06:56:06**,
`m11-bench-xrun-quick.json` **06:56:29**. Session span (build 05:47 → last artifact 06:57):
**2026-10-14 05:47–06:57 UTC** (= 2026-10-14 13:47–14:57 +0800). The 1-min load across the session
ranged **1.12 → 7.18**.

### 3.1 Why the interpreter control is `--quick` (a measured at-this-commit fact)

At this commit the **interpreter tier is pathologically slow on the `foreign` workload's cross-module
calls** — and the cost is *super-linear* in the loop count `N`, not a constant multiplier. Direct
`beam_rt -pa <scratch> run <drv.beam> <fn>/1 <N>` probes of the case-1 loop
(`byte_size(cowboy_bstr:to_lower/1)` per iteration), same machine, same staged modules:

| tier | N=500 | N=1 000 | N=2 000 | N=4 000 | N=20 000 |
|---|---|---|---|---|---|
| default (baseline JIT) | 35 ms | — | 64 ms | — | 514 ms |
| **interpreter** | 102 ms | 158 ms | 219 ms | 711 ms | **84 088 ms** |

The m10 record's interpreter pass measured the same case at **21 959 ns/call** (i.e. ~0.44 s at
N = 20 000) and completed the whole six-workload `--full` tier control in **41 s**; the same case now
costs **4 178 832 ns/call** (`m11-bench-xrun-interp.json`), a ~190× interpreter regression on this shape,
and a `--full` interpreter pass would need **~50 min** of wall time on this host alone. The
`--quick` form (3 samples/case) was therefore used for run B, and the matched-form default-tier run B2
exists to keep the tier comparison like-for-like. The interpreter tier is **not** on `beam_rt`'s default
path (`BEAM_RT_TIER=interpreter` is the only kill switch); this is recorded as a measurement fact, not
diagnosed or fixed here.

## 4. Per-workload status (full cross-runtime pass, `m11-bench-xrun.json`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust | cases |
|---|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` | — |
| `slice`      | compute   | `ok` | `ok` | 9/9 ok on each side |
| `foreign`    | compute   | `ok` | `ok` | 4/4 ok on each side |
| `list_walk`  | compute   | `ok` | `ok` | 1/1 ok on each side |
| `copy_bound` | messaging | `ok` | `ok` | **4/4 ok on each side** (`etf_roundtrip`, `ets_copy`, `send_recv`, `send_recv_plain`) |
| `gc_bound`   | gc        | `ok` | `ok` | 1/1 ok on each side |

No rust row is fabricated: every rust row carries real `Summary`/scalar metrics (no `null`-only rows, no
`unsupported` classification, no crash/exit-3). Every copy/churn/walk case validated its own round-trip
with a bound-variable pattern on both sides and printed the deterministic constant (recorded in the
report's `observed stdout:` notes), so the two runtimes executed identical work.

The tier-effect control (`m11-bench-xrun-interp.json`) is **6 rust rows, all `ok`** (no `sys` rows by
construction); the matched control (`m11-bench-xrun-quick.json`) is the same 6 rows, all `ok`. The full
external pass (`m11-bench-external.json`) is **4 rows, all `ok`**. `m11-bench-plb.json` is **1 `ok`**
row; `m11-bench-emqx.json` is **1 `skipped`** row (no metric).

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`rust/sys` > 1 means the Rust side costs **more** per unit (its ns value is higher). The marginals
(`*_ns_*`) are the load-robust cross-runtime signal; the wall/startup rows are inflated by the load
above and are shown for completeness. **Both sides of every ratio are named** — `sys` is the system
ERTS (OTP 29.1.1 / erts-17.1), `rust` is `beam_rt` at its **default** (baseline-JIT) tier.

| workload | headline cross-runtime metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 112.8126 ms | 15.2122 ms | **0.135×** (rust 7.42× faster) |
| `baseline`   | `vm_memory_total_bytes` | 36 221 440 B | 805 458 B | **0.022×** (rust 44.97× lower) |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 118.79–122.52 ms | 15.35–37.85 ms | **0.127×–0.311×** (rust faster) |
| `foreign`    | `case_<name>_ns_per_call` | 210.23–344.97 ns | 19 306.39–77 167.32 ns | **60.2×–367.1×** (rust higher) |
| `list_walk`  | `case_list_walk_ns_per_element` | 2.4194 ns | 45.9731 ns | **19.0×** (rust higher) |
| `copy_bound` | `case_<name>_ns_per_op` | 236.93–1 453.37 ns | 2 435.07–21 522.24 ns | **10.3×–30.7×** (rust higher) |
| `gc_bound`   | `case_churn_ns_per_op` | 4 270.13 ns | 128 524.45 ns | **30.1×** (rust higher) |

### 5.1 Per-case detail

`baseline` (whole-process cost — startup latency is the trustworthy cross-runtime comparison):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `startup_ms` (mean) | 112.8126 | 15.2122 | 0.135× |
| `compute_loop_ms` (mean) | 176.1818 | 58.6524 | 0.333× |
| `compute_ops_per_sec` | 5 675 956.53 | 17 049 597.15 | 3.00× (rust faster; native floor, not interpreter throughput) |
| `vm_memory_total_bytes` | 36 221 440 | 805 458 | 44.97× |
| `peak_memory_bytes` (RSS) | 135 761 920 | 31 793 152 | 4.27× |

`slice` (one call per process: `case_<name>_wall_ms` mean, and the comparable per-process memory):

| case | sys wall_ms | rust wall_ms | rust/sys | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|
| `add` | 120.6590 | 15.3536 | 0.127× | 66.39 | 14.50 |
| `fib` | 118.8547 | 15.6304 | 0.132× | 66.49 | 15.20 |
| `pick` | 118.7913 | 15.4627 | 0.130× | 66.11 | 15.16 |
| `calendar_last_day` | 121.8355 | 16.3392 | 0.134× | 66.44 | 16.04 |
| `queue_from_size` | 120.5272 | 21.0184 | 0.174× | 66.46 | 17.99 |
| `gb_trees_size` | 119.7834 | 16.5597 | 0.138× | 66.34 | 16.01 |
| `base64_encode_size` | 120.5784 | 17.2272 | 0.143× | 66.27 | 17.20 |
| `ordsets_from_size` | 122.5184 | 15.9639 | 0.130× | 66.22 | 15.75 |
| `string_str_length` | 121.5968 | 37.8461 | 0.311× | 66.36 | 26.30 |

`slice` whole-workload `peak_memory_bytes`: sys 71 741 440 → rust 27 758 592 (**2.58×** lower for rust).

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys | sys startup p50 (ms) | rust startup p50 (ms) | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|---|
| `bstr_to_lower` | 344.9711 | 23 751.0676 | 68.85× | 118.62 | 22.99 | 66.64 | 74.53 |
| `bstr_capitalize_token` | 324.9957 | 28 757.0043 | 88.48× | 118.70 | 15.42 | 66.12 | 76.95 |
| `emqx_base62_encode` | 210.2289 | 77 167.3229 | 367.06× | 114.42 | 15.33 | 66.28 | 62.68 |
| `emqx_base62_decode` | 320.8242 | 19 306.3883 | 60.18× | 123.04 | 15.13 | 66.49 | 63.51 |

`list_walk` (BIF-free recursion — the per-element dispatch signal):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 2.4194 | 45.9731 | 19.00× |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 | (per-runtime `N`, not a ratio) |
| `case_list_walk_peak_rss_bytes` p50 | 66.34 MiB | 15.21 MiB | rust lower |

`copy_bound` (term-copy boundary; **four** cases — `send_recv_plain` is new at m11):

| case | sys ns/op | rust ns/op | rust/sys | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|
| `etf_roundtrip` | 650.4860 | 7 033.3943 | 10.81× | 66.21 | 19.25 |
| `ets_copy` | 236.9251 | 2 435.0664 | 10.28× | 66.91 | 90.39 |
| `send_recv` | 1 453.3740 | 21 522.2389 | 14.81× | 66.57 | 21.05 |
| `send_recv_plain` | 653.7492 | 20 075.2422 | 30.71× | 66.64 | 21.16 |

`copy_bound` whole-workload `peak_memory_bytes`: sys 71 970 816 → rust 94 973 952 (**1.32×** *higher* for
rust — the `ets_copy` row is the one that scales with `N`, see the honesty check).

`gc_bound`:

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_churn_ns_per_op` | 4 270.1253 | 128 524.4542 | 30.10× |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 | (per-runtime `N`) |
| `case_churn_peak_rss_bytes` p50 | 66.66 MiB | 15.43 MiB | rust 4.32× lower |

## 6. Tier effect (Rust vs Rust — **not** a system-ERTS comparison)

`rust` default tier / `rust` interpreter tier, read from the **matched-form `--quick`** pair
(`m11-compare-tier-effect-quick.txt`: `a` = interpreter, `b` = default). `> 1` means the default
(baseline-JIT) tier is **cheaper**; `< 1` means the interpreter is cheaper.

| workload | metric | interpreter | default (baseline JIT) | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 17.3247 | 37.5050 | 0.462× (default dearer — see note) |
| `baseline` | `compute_loop_ms` | 63.6089 | 99.0786 | 0.642× (noise; native `lists:seq/sum` floor) |
| `list_walk` | `ns_per_element` | 159.6008 | 47.4465 | **3.36×** (default cheaper) |
| `gc_bound` | `churn` ns/op | 243 176.0348 | 123 688.7103 | **1.97×** (default cheaper) |
| `copy_bound` | `etf_roundtrip` ns/op | 6 759.9803 | 7 496.9099 | 0.90× (interpreter marginally cheaper) |
| `copy_bound` | `ets_copy` ns/op | 3 313.9438 | 2 344.7269 | **1.41×** (default cheaper) |
| `copy_bound` | **`send_recv` ns/op** | 22 654.6925 | **22 054.4402** | **1.03× — parity** |
| `copy_bound` | **`send_recv_plain` ns/op** | 18 662.4402 | **19 330.4250** | **0.97× — parity** |
| `foreign` | 4 cases ns/call | 2 832 121–4 178 832 | 19 191–76 967 | **50×–177×** (default cheaper — **interpreter pathology, §3.1**) |
| `slice` | 9 cases wall_ms | 20.5–33.8 | 17.1–42.1 | 0.79×–1.49× (mixed; startup noise, no stable tier effect) |

**The requested `copy_bound` rows, with direction:**
- **`send_recv`** (`a` = interpreter): **22 654.6925 → 22 054.4402 ns/op, Δ = −600.25 (−2.65 %)** — the
  default tier is now marginally *cheaper*; the two tiers are at **parity**.
- **`send_recv_plain`**: **18 662.4402 → 19 330.4250 ns/op, Δ = +667.98 (+3.58 %)** — the default tier is
  marginally *dearer*; again **parity** (the two cases differ by ~3.6 % in *both* directions, i.e. within
  run-to-run scatter).

These two cases are the ones with a genuinely matched, same-tier sample count, so the parity reading is
solid. The other matched marginals agree on the default tier's direction: `list_walk` `ns_per_element`
159.6008 → 47.4465 (**−70.3 %**, default 3.36× cheaper), `gc_bound` churn 243 176.0348 → 123 688.7103
(**−49.1 %**, default 1.97× cheaper), `copy_bound` `ets_copy` −29.3 % and `etf_roundtrip` **+10.9 %**
(the one metric where the interpreter is marginally cheaper). `slice` shows **no stable tier effect**:
its per-case `wall_ms` includes a whole process startup per case, and in the matched pair 3 of 9 cases
are 32–33 % cheaper under the default tier while 2 are ~25–26 % *dearer*, with the rest inside ±10 % —
that spread tracks the per-process startup noise of the two `--quick` runs, not the tier.

> **The m10 "inverted" row is gone.** At m10 the same matched comparison had `send_recv` at
> **36 969.39 → 79 518.12 ns/op (default 2.15× *dearer*)**, a repeatable baseline-JIT message-passing
> cost. At m11 it is **22 054 vs 22 655 ns/op — parity**. This is a Rust-vs-Rust tier observation only;
> the Rust-vs-system ratios are in §5.
> **Read the `baseline` rows of this table with care:** the matched `--quick` control happened to sample
> `startup_ms` (37.5 ms) far above the `--full` cross-runtime run's 15.2 ms and the interpreter run's
> 17.3 ms, i.e. that row is dominated by one noisy sample set on a shared host, *not* by a tier effect.
> `compute_loop_ms`'s spread is the same story (the loop is served natively on both sides).
> **`foreign`'s 50×–177× rows are not a "JIT win"** — they are the plain-JIT baseline against the
> pathological interpreter of §3.1; do not read them as a normal tier effect.

The as-specified artifact `m11-compare-tier-effect.txt` (interpreter `--quick` vs the default `--full`
`m11-bench-xrun.json`) gives the same directions: `send_recv` 22 654.6925 → 21 522.2389 (−5.00 %),
`send_recv_plain` 18 662.4402 → 20 075.2422 (+7.57 %), `list_walk` −71.19 %, `gc_bound` −47.15 %,
`foreign` −98.0 % to −99.4 %, and the `sys` rows `only-in-b` by construction.

## 7. Sys-only external targets (`m11-bench-external.json`, `--full`)

| workload | status | headline | sys peak RSS |
|---|---|---|---|
| `erlc_compiler` | `ok` | wall 7 905.65 ms; 7.589 files/s (60 produced over 46 503 LOC) | 135 045 120 B |
| `elixir_compiler` | `ok` | wall 47 667.62 ms; 4.678 files/s (105 attempted / 223 produced); compiler startup 324.33 ms | 205 479 936 B |
| `cowboy` | `ok` | 190 626.72 req/s; p50 0.0787 ms / p99 0.2234 ms (953 510 requests, 0 errors); build 45 207.25 ms | 158 814 208 B |
| `khepri` | `ok` | `rebar3 eunit` wall 83 402.90 ms | 357 818 368 B |
| `plb` (`m11-bench-plb.json`) | `ok` | wall 68 318.69 ms; `test_time_ms` p50 559.98 / p99 3 355.48 (n=10); resolved `dotnet` 9.0.318 | 169 824 256 B |
| `emqx` (`m11-bench-emqx.json`) | `skipped(emqx-requires-otp-28)` | no number | — |

**Every one of these rows is executed by its own system-ERTS toolchain** (`erlc`, `elixirc`, the Cowboy
release on the oracle `erl`, `rebar3`'s OTP-28 Erlang, the `.NET` tool for PLB). **None** is executed by
the Rust runtime — they are **reference** figures for the sys-only surfaces, not Rust-runtime results.

## 8. m11 vs m10 (from `m11-compare-vs-m10-xrun.txt` / `m11-compare-vs-m10-external.txt`)

The tool diffs the **mean**; `a` = m10, `b` = m11. Rust-default-tier rows first (the interesting ones):

| (workload, metric) | m10 | m11 | Δ |
|---|---|---|---|
| `copy_bound` `send_recv` ns/op **rust** | 79 518.1199 | 21 522.2389 | **−72.93 %** |
| `copy_bound` `send_recv` wall_ms **rust** | 175.8221 | 58.4826 | −66.74 % |
| `copy_bound` `etf_roundtrip` ns/op **rust** | 9 838.0045 | 7 033.3943 | −28.51 % |
| `copy_bound` `ets_copy` ns/op **rust** | 3 267.8549 | 2 435.0664 | −25.48 % |
| `gc_bound` `churn` ns/op **rust** | 147 194.3719 | 128 524.4542 | −12.68 % |
| `list_walk` ns/element **rust** | 47.3880 | 45.9731 | −2.99 % |
| `foreign` `bstr_to_lower` ns/call **rust** | 17 789.3946 | 23 751.0676 | **+33.51 %** |
| `foreign` `bstr_capitalize_token` ns/call **rust** | 22 566.6138 | 28 757.0043 | **+27.43 %** |
| `foreign` `emqx_base62_decode` ns/call **rust** | 16 325.9957 | 19 306.3883 | **+18.26 %** |
| `foreign` `emqx_base62_encode` ns/call **rust** | 16 601.0457 | 77 167.3229 | **+364.83 %** |
| `baseline` `startup_ms` **rust** | 15.5243 | 15.2122 | −2.01 % |
| `baseline` `vm_memory_total_bytes` **rust** | 805 458 | 805 458 | 0.00 % |
| `copy_bound` `send_recv_plain` (all rows) | — | — | `metric-only-in-b` (the new case) |

`sys`-side rows moved only with the host (`foreign` ns/call +4.2 %/−0.3 %/+9.2 %/+1.6 %; `list_walk`
ns/element +12.9 %; `gc_bound` churn −2.2 %; `copy_bound` ns/op −1.5 % to −0.0 %; `baseline` startup
−1.35 %), which is the cross-check that the interesting rust moves are not oracle drift.

**Two rust-side moves worth flagging (recorded, not diagnosed here):**
1. **`copy_bound` `send_recv` is 3.7× cheaper than at m10** (79 518 → 21 522 ns/op) and `etf_roundtrip`
   28.5 % cheaper — i.e. the m10 baseline-JIT message-passing imbalance (§6) is closed at this commit,
   consistent with the baseline tier having lowered `Send`/the receive-control family natively.
2. **`foreign` `emqx_base62_encode` is 4.6× dearer than at m10** (16 601 → 77 167 ns/call) while its
   oracle row is flat (210.88 → 210.23, −0.31 %); the other three `foreign` rust rows are 18–34 % dearer.
   The matched `--quick` control reproduces it (76 967 ns/call), so it is reproducible on this host and
   not a one-off sample.

External targets (m10 → m11): `erlc_compiler` wall 6 785.46 → 7 905.65 ms (**+16.51 %**);
`elixir_compiler` wall 47 806.41 → 47 667.62 ms (−0.29 %); `cowboy` p50 0.063634 → 0.078693 ms
(**+23.66 %**) and `requests_per_sec` 210 250.49 → 190 626.72 (**−9.33 %**); `khepri` wall 78 762.66 →
83 402.90 ms (+5.89 %); `plb` wall 70 841.43 → 68 318.69 ms (−3.56 %). All are on the shared host and
none is a like-for-like quiet-host comparison; `cowboy`'s p50 rose while its own p99 *fell*
(0.301994 → 0.223380 ms), which reads as host contention rather than a server change.

## 9. Runtime boot status at this commit (step-5 factual probe)

```
$ erts_rust/target/release/beam_rt -boot bootstrap/bin/start
exit_code=3
beam_rt: unsupported: opcode: run: process blocked (no message available)
```

Exit code **3** with that single `stderr` line — **identical to the pre-batch commit's recorded status**
(the `-boot` probe still stops on `run: process blocked (no message available)`). Recorded as fact; no
attempt was made to change it.

## 10. Honesty check

- Rust rows: all `ok` with real metrics; none `unsupported`, none fabricated, no workload crashed.
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`);
  each run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N`, so per-process startup (including the Rust runtime's code-path
  resolution) cancels on both sides.
- `copy_bound`'s `send_recv_plain` is a **new** case at m11: it reports real metrics on both runtimes and
  is `metric-only-in-b` in the m10 comparison (there is nothing to compare it against).
- The tier tables are labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS) and
  `rust` (`beam_rt` default tier) explicitly. The interpreter run is `--quick` while the cross-runtime run
  is `--full`; the mismatched pair `m11-compare-tier-effect.txt` is accompanied by the **matched-form**
  `m11-compare-tier-effect-quick.txt` for exactly that reason.
- **Two rows are explicitly *not* like-for-like and are labelled as such:** `foreign`'s rust peak RSS
  (77.18 MiB vs sys 68.27 MiB) and `copy_bound`'s rust peak RSS (90.57 MiB vs sys 68.64 MiB) scale with the
  per-runtime loop count `N`, whereas `slice` (one call per process) is the comparable per-process memory
  metric (`slice` rust 26.47 MiB vs sys 68.42 MiB).
- `emqx` is the only non-`ok` row, and it carries a verified reason and **no** number.
- All committed artifact paths are normalised (no worktree/`$TMPDIR` absolutes).
