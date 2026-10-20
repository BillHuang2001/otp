# m18 — end-to-end measurement session

The suite-wide measurement record for the m18 session: a full re-run of the `erts_rust/tools/bench`
series, the regression analysis against the recorded m17/Q figures, and the **load-proof binary A/B**
of the one code change the session made (the `beam_gc` live-set nursery policy, `MEASURED.md` §*The
nursery follows the live set*).

`erts_rust/PERF.md` does not exist in this tree, so this file is the m18 record and the text to land
there. Raw `Report` JSON is committed beside it under `results-m18/`; every metric value in it is
untouched (only the mechanical `runtimes[].program` / `$TMPDIR` path normalisations).

## 0. The two sides, named

| side | program | version |
|---|---|---|
| **sys** | the oracle `erl` on `PATH` | **Erlang/OTP 29.1.1 / erts-17.1** |
| **rust** | `erts_rust/target/release/beam_rt`, default baseline-JIT tier (`JitMode::Auto` ⇒ `Tier::BaselineJit`; `BEAM_RT_TIER=interpreter` is the only kill switch) | reports `30` |

Every `rust/sys` figure below compares those two. The binary A/B (§4) compares **two `beam_rt`
builds** and is never a system-ERTS comparison.

## 1. Host, load and the quiet window

14 logical cores, shared with other agents, no `/usr/bin/time`. Every matched pass and every A/B pass
is pinned with `taskset -c 3`; `/proc/loadavg` was recorded before and after **every** pass
(`results-m18/m18-load.log`). The session spanned load **1.3 → 92**:

| phase | artifacts | 1-min load across the passes |
|---|---|---|
| pre-change matched rounds r1–r3 | `xab-*` (not archived) | 9.09 → **92.28** |
| post-change matched rounds r4–r7 (intermediate tree) | not archived | 17.53 → 28.35 |
| binary A/B, intermediate tree vs pre-change | `ab2-*` predecessor (not archived) | 15.21 → 12.51 |
| external targets, loaded window | `ext-full.json`, `erlc-r{1,2,3}.json` | 11.92 → 19.42 / 18.75 → 17.85 |
| **binary A/B, final tree** | `ab2-pre-r{1,2,3}.json`, `ab2-post-r{1,2,3}.json` | **3.93 → 1.33** |
| **external targets, quiet window** | `ext-full-quiet.json` | **1.33 → 3.84** |
| **final matched rounds r8–r10** | `fin-rust-r{8,9,10}.json`, `fin-sys-r{8,9,10}.json` | **3.53 → 2.27** |

**The record is the quiet window (§3–§6).** Under load 9–92 the *rust* side absorbs the variance and
the same per-round ratio moves several-fold (churn read 12.52 / 14.33 / 15.75 / 13.61 across r4–r7 at
load 17–33), which is exactly the documented behaviour of the shared host. The loaded phases are kept
as provenance, not as figures.

## 2. Noise floor of the instruments used here

- **Matched-round ratio at quiet load (1.3–3.9):** the three rounds' per-round ratios reproduce to
  within **±2 %** for every row except `churn` (±2.6 %) and `bstr_capitalize_token` (±2 %). That ±2 %
  is the noise floor: no cross-runtime movement below ~2 % is claimed from a matched round.
- **Paired binary A/B at quiet load:** per-round ratios span **±4 %** (churn 0.846–0.915, `ets_copy`
  0.896–0.951, `send_recv` 0.899–0.946, `etf_roundtrip` 1.041–1.085). A change is claimed only when
  the sign is consistent across all three rounds **and** the median clears that band.
- The per-unit figures themselves are the driver's **paired, startup-cancelled marginal** — each round
  runs the `N`-unit pass then the `0`-unit pass back to back and takes the median of the paired
  differences, withheld below `3 × max(1 ms, 1.4826·MAD)`. Nothing was withheld in any m18 pass.

## 3. The headline — final matched cross-runtime rounds (r8, r9, r10; load 1.3–3.5)

Six passes = **three matched rounds** (r8 rust→sys, r9 sys→rust, r10 rust→sys): the side order
alternates round by round and only one process runs at a time. Ratio > 1 ⇒ rust dearer. `N` is per
runtime, so only the per-unit normalisation is comparable.

| row | rust | sys | **rust/sys** | per-round |
|---|---|---|---|---|
| `baseline` startup `p50` (ms) | 272.0 / 271.6 / 271.5 | 103.7 / 103.6 / 103.6 | **2.62×** | 2.62 2.62 2.62 |
| `baseline` startup `p99` (ms) | 273.1 / 273.7 / 273.1 | 104.0 / 104.2 / 104.0 | **2.63×** | 2.63 2.63 2.63 |
| `baseline` compute loop `p50` (ms) | 294.7 | 164.5 | 1.79× | (r8 only) |
| `slice` wall median (ms, N=1) | 273.1 / 273.3 / 273.6 | 114.6 / 114.9 / 114.5 | **2.38×** | 2.38 2.38 2.39 |
| `list_walk` ns/element | 18.08 / 17.93 / 18.19 | 2.306 / 2.317 / 2.372 | **7.74×** | 7.84 7.74 7.67 |
| `copy_bound/etf_roundtrip` ns/op | 1 509 / 1 492 / 1 492 | 661.0 / 658.1 / 659.4 | **2.27×** | 2.28 2.27 2.26 |
| `copy_bound/ets_copy` ns/op | 1 601 / 1 597 / 1 622 | 222.7 / 224.5 / 222.3 | **7.19×** | 7.19 7.12 7.29 |
| `copy_bound/send_recv` ns/op | 5 864 / 5 877 / 5 879 | 1 510 / 1 500 / 1 497 | **3.92×** | 3.88 3.92 3.93 |
| `copy_bound/send_recv_plain` ns/op | 2 624 / 2 609 / 2 600 | 672.8 / 674.1 / 666.0 | **3.90×** | 3.90 3.87 3.90 |
| `gc_bound/churn` ns/op | 56 190 / 58 540 / 57 290 | 4 234 / 4 244 / 4 257 | **13.46×** | 13.27 13.79 13.46 |
| `foreign/bstr_to_lower` ns/call | 3 194 / 3 088 / 3 231 | 343.7 / 330.5 / 333.4 | **9.34×** | 9.29 9.34 9.69 |
| `foreign/bstr_capitalize_token` ns/call | 4 164 / 3 994 / 4 021 | 312.9 / 323.4 / 316.8 | **12.69×** | 13.31 12.35 12.69 |
| `foreign/emqx_base62_encode` ns/call | 3 337 / 3 317 / 3 298 | 210.3 / 201.7 / 211.5 | **15.87×** | 15.87 16.45 15.59 |
| `foreign/emqx_base62_decode` ns/call | 5 506 / 5 594 / 5 706 | 287.9 / 285.2 / 286.2 | **19.61×** | 19.12 19.61 19.94 |
| `binary_append` ns/call | 1 333 000 / 1 314 000 / 1 326 000 | 81 910 / 81 800 / 82 370 | **16.10×** | 16.28 16.07 16.10 |

`slice` per case (r8, ms): `add` 273.1/114.3, `fib` 272.6/114.7, `pick` 273.1/114.5,
`calendar_last_day` 274.4/115.1, `queue_from_size` 273.4/115.0, `gb_trees_size` 272.1/114.6,
`base64_encode_size` 275.7/115.4, `ordsets_from_size` 273.0/114.6, `string_str_length` 278.5/114.5
(rust/sys).

Deterministic companions (r8, rust / sys): `etf_roundtrip_ops` 500 000 / 1 500 000, `ets_copy_ops`
250 000 / 2 000 000, `send_recv_ops` 90 000 / 500 000, `send_recv_plain_ops` 300 000 / 1 000 000,
`churn_ops` 10 000 / 200 000, `list_walk_elements` 50 000 000 / 100 000 000, `foreign` calls
50 000/1 500 000 · 40 000/1 500 000 · 60 000/3 000 000 · 60 000/3 000 000, `binary_append_calls`
2 000 / 50 000 (`binary_append_bytes` 131 072 000 / 3 276 800 000). These are contention-immune —
**quote them, not the wall clock, on a loaded host**.

Memory (r8, like-for-like only where stated):

| workload | rust peak RSS (MiB) | sys peak RSS (MiB) | rust/sys |
|---|---|---|---|
| `baseline` | 146.08 | 128.98 | 1.13× |
| **`slice` (N=1 — the like-for-like row)** | **116.99** | **64.79** | **1.81×** |
| `list_walk` | 114.38 | 64.51 | 1.77× |
| `copy_bound` | 152.89 | 64.63 | 2.37× |
| `foreign` | 115.96 | 64.56 | 1.80× |
| `gc_bound` | 114.34 | 64.60 | 1.77× |
| `binary_append` | 114.74 | 64.40 | 1.78× |

`erlang:memory(total)` (`vm_memory_total_bytes`): rust **7.74 MB** vs sys **36.20 MB**. That is an
**accounting** figure, not a footprint (see `../BOTTLENECKS.md` §6) — the like-for-like footprint row is
`slice`'s peak RSS.

## 4. The change — the load-proof binary A/B

`ab2-{pre,post}-r{1,2,3}.json`: the **same `bench` binary** driving two `beam_rt` builds (`pre` = the
tree at `5ddf6e72e4` before the `beam_gc` change, `post` = this session's final `beam_rt`), rust-only
`--quick`, **alternating the side order round by round** (pre→post, post→pre, pre→post), pinned to
core 3, at load **1.3–3.9**. This isolates the code change from host load; it is the only valid
"did this change help" instrument.

| rust-only case | pre (`5ddf6e72e4`) | post (HEAD) | **post/pre** | per-round |
|---|---|---|---|---|
| `gc_bound/churn` ns/op | 64 224 / 64 321 / 66 325 | 58 762 / 56 058 / 56 104 | **0.871×** (12.9 % faster) | 0.915 0.871 0.846 |
| `copy_bound/ets_copy` ns/op | 1 756.0 / 1 678.6 / 1 760.5 | 1 616.3 / 1 595.9 / 1 577.8 | **0.920×** | 0.920 0.951 0.896 |
| `copy_bound/send_recv` ns/op | 6 249.7 / 6 488.7 / 6 327.0 | 5 911.7 / 5 833.1 / 5 882.1 | **0.930×** | 0.946 0.899 0.930 |
| `copy_bound/send_recv_plain` ns/op | 2 604.1 / 2 633.8 / 2 587.8 | 2 606.1 / 2 612.9 / 2 585.7 | 0.999× (parity) | 1.001 0.992 0.999 |
| `copy_bound/etf_roundtrip` ns/op | 1 441.0 / 1 419.7 / 1 398.5 | 1 499.6 / 1 505.0 / 1 516.9 | **1.060×** (6 % dearer) | 1.041 1.060 1.085 |
| `gc_bound` peak RSS (MiB) | 114.0 / 114.1 / 114.1 | 114.4 / 114.4 / 114.4 | **+0.3 MiB (+0.3 %)** | — |
| `copy_bound` peak RSS (MiB) | 152.5 / 152.4 / 152.4 | 152.8 / 152.9 / 152.9 | **+0.4 MiB (+0.3 %)** | — |

The causal structural counters (deterministic, contention-immune, `tests/churn_attribution.rs`, 4 000
ops; full table in `MEASURED.md`): **collections/op 1.500 → 0.201** (7.5× fewer; 0.167 minor / 0.034
major), **words copied/op 1 283.5 → 271.0** (4.7× less), **objects copied/op 425.01 → 103.38** (4.1×
less), words reclaimed/op unchanged at 806.5, peak nursery spaces still **1** (no major chaining), at a
cost of peak resident (nursery + mature) **3 195 → 15 143 words** (+47 KiB) and the +0.3–0.4 MiB of
peak RSS above. An earlier intermediate tree of the same change read 1.138× (spread 1.058–1.170) on the
same A/B at load 6–33; the final tree reads **1.148×** (0.846–0.915) in the quiet window — the two
windows agree.

**Noise floor and honesty for this claim:** ±4 % per paired round at this load (§2); the sign is
consistent in all three rounds and the median (0.871) is well outside the spread, so the churn win is
real. `send_recv_plain`'s 0.999× is parity — the change does **not** explain that row's standing gap
(§7). `etf_roundtrip`'s 1.060× is a **real, small regression** costing it, and is reported rather than
buried (§7).

## 5. External targets (all `sys`-only, staged in a read-only `$TMPDIR` copy)

Two windows, both measured in this session. The loaded window is included precisely so the quiet
figures can be read as *not* a code change:

| target | **quiet window** (load 1.3–3.8, `--full`) | loaded window (load 12–19, `--full`) | repeats (load ~18, `--full`) |
|---|---|---|---|
| `erlc_compiler` | **9.664 files/s**, 7 489.9 loc/s, wall 6 209 ms (60 files, 46 503 LOC) | 6.550 files/s, 5 076.7 loc/s, wall 9 160 ms | 6.118 / 6.376 / 6.094 files/s |
| `elixir_compiler` | **4.680 files/s**, 2 349.7 loc/s, wall 47 650 ms (105 sources → 223 beams), **compiler start-up 311.0 ms** | 4.187 files/s, wall 53 258 ms, start-up 477.3 ms | — |
| `cowboy` (hello_world) | **183 201 req/s**, 916 405 requests, **0 errors**, latency **p50 0.0834 / p90 0.1169 / p99 0.2405 ms**, build 44 317 ms | 109 058 req/s, 545 595 requests, 0 errors, p50 0.0911 / p90 0.2454 / p99 1.2446 ms, build 62 590 ms | — |

The `elixirc` **compiler start-up** is the report's own control for host starvation: 311.0 ms quiet vs
477.3 ms loaded vs m16's 288.1 ms — so the loaded `erlc`/`elixir`/`cowboy` figures are host-starved, and
the quiet ones are the comparable numbers. `khepri` (`rebar3`) and `plb` (`dotnet` ≥ 9) were not in
this round's set; `emqx` remains an honest `skipped(emqx-requires-otp-28)`.

## 6. Regression analysis against the recorded figures

### 6.1 Against the recorded **m17** session (`erts_rust/tools/bench/RESULTS.md`, load 3–92)

| row | m17 | **m18** | movement |
|---|---|---|---|
| startup `p50` | 2.51× | **2.62×** | +4.4 % (inside the loaded m17 window's own spread) |
| startup `p99` | 2.56× | **2.63×** | +2.7 % |
| `slice` | 2.63× | **2.38×** | −9.5 % |
| `list_walk` | 11.80× | **7.74×** | −34 % |
| `etf_roundtrip` | 2.21× | **2.27×** | +2.7 % |
| `ets_copy` | 9.28× | **7.19×** | −22.5 % |
| `send_recv` | 5.39× | **3.92×** | −27 % |
| `send_recv_plain` | 3.16× | **3.90×** | **+23 %** — see §7.1 |
| `gc_bound/churn` | 18.75× | **13.46×** | −28 % (this change accounts for −12.9 %, §4) |
| `bstr_to_lower` | 18.81× | **9.34×** | −50 % |
| `bstr_capitalize_token` | 22.46× | **12.69×** | −43 % |
| `emqx_base62_encode` | 20.04× | **15.87×** | −21 % |
| `emqx_base62_decode` | 25.88× | **19.61×** | −24 % |
| `binary_append` | 40.02× | **16.10×** | −60 % (not attributable to this change — `binary_append` is not in the A/B) |

Only `send_recv_plain` moved the wrong way, and the A/B **exonerates this change** on it (0.999×).

### 6.2 Against the quiet **Q** record (`crates/beam_engine/MEASURED.md`, load 1.02–2.37 — the closest
load match to m18's 1.3–3.5, so the most meaningful comparison in this file)

Sys-side absolutes agree element for element (e.g. `send_recv_plain` 669.20 → 672.8/674.1/666.0 ns,
`ets_copy` 222.47 → 222.3/224.5/222.7 ns, `churn` 4 257.75 → 4 234/4 244/4 257 ns, `list_walk` 2.34 →
2.31/2.32/2.37 ns), which is what makes the rust-side deltas readable as code movement rather than load:

| rust case | Q | **m18** | movement |
|---|---|---|---|
| `list_walk` ns/element | 18.03 | 18.08 / 17.93 / 18.19 | parity |
| `etf_roundtrip` ns/op | 1 433.16 | 1 492 / 1 509 / 1 492 | **+4.5 %** (this change: +6 %) |
| `ets_copy` ns/op | 2 180.34 | 1 597 / 1 601 / 1 622 | **−26 %** |
| `send_recv` ns/op | 8 154.48 | 5 864 / 5 877 / 5 879 | **−28 %** |
| `send_recv_plain` ns/op | 1 965.28 | 2 600 / 2 609 / 2 624 | **+33 %** — §7.1 |
| `churn` ns/op | 62 478.60 | 56 190 / 58 540 / 57 290 | **−8 %** (this change: −12.9 % vs pre 64.2–66.3 k) |
| `bstr_to_lower` ns/call | 3 174.91 | 3 088 / 3 194 / 3 231 | parity |
| `bstr_capitalize_token` ns/call | 4 136.55 | 3 994 / 4 021 / 4 164 | parity |
| `emqx_base62_encode` ns/call | 3 343.09 | 3 298 / 3 317 / 3 337 | parity |
| `emqx_base62_decode` ns/call | 5 574.70 | 5 506 / 5 594 / 5 706 | parity |

So since Q: `ets_copy` −26 %, `send_recv` −28 %, `churn` −8 % (this session's change owns −12.9 % of the
pre-change figure, i.e. all of it), everything else at parity — **except `send_recv_plain` (+33 %) and
`etf_roundtrip` (+4.5 %)**.

## 7. What was measured and could **not** be explained or attributed

### 7.1 `send_recv_plain` is 33 % dearer on rust than the Q record, and this change did not cause it
Q (quiet, same instrument): sys **669.20 ns/op** → rust **1 965.28 ns/op** = 2.91×.
m18 (quiet): sys **672.8 / 674.1 / 666.0** → rust **2 624 / 2 609 / 2 600** = **3.90×**.
**The sys side is identical and the rust side is +33 %.** The paired binary A/B puts the session's
`beam_gc` change at **0.999×** on this exact case (1.001 / 0.992 / 0.999), so the movement lies
**between** the Q tree (2026-10-20) and this session's base commit `5ddf6e72e4` — i.e. it is a
pre-existing rust-side regression on the **delivery-only, no-ETF two-process** path that no published
A/B has covered. **Handoff, unattributed.** The two `send_recv` cases differ only in whether the
receiver re-encodes the payload, so the *attribution* figure `send_recv − send_recv_plain` (which was
the reason these two cases exist) is now distorted by it: 3.27 µs/op (rust) vs 0.84 µs/op (sys) — read
`send_recv` alone, not the difference, until this is root-caused.
Note the known literal-sharing asymmetry (`beam_message` shares a literal-area payload; the oracle's
message path passes `litopt = NULL` and copies it), which makes this row a wrapper-plus-ping-pong
comparison rather than identical work — but that asymmetry is **static** and cannot explain a change of
sign between two rust builds of the same source.

### 7.2 `etf_roundtrip` is 6 % dearer than the pre-change build — real, small, mechanism not isolated
The A/B is consistent 3/3 at a quiet load (1.041 / 1.060 / 1.085, median 1.060), which is outside this
session's ±4 % A/B band, so it is a real cost of the change. Candidate mechanism, **not verified**: the
mature generation is now sized at 2× its post-fullsweep need instead of ~1.06×, so a fullsweep installs
and touches a larger space; `etf_roundtrip` allocates a fresh result term per op through the BIF-result
copy-out and is the shortest-lived shape in the set. The session's `beam_gc` owner measured the knob as
one constant (`MATURE_TARGET_LIVE_MULTIPLE`, 1 reproduces the pre-change figures exactly). **Not
reverted**: `ets_copy` (−8 %), `send_recv` (−7 %) and `churn` (−12.9 %) all move the other way, so the
net of the change is strongly positive; the trade is recorded rather than hidden.

### 7.3 Startup is 2.62× and moved +4 % against m17
Absolute: rust **271.5–272.0 ms** vs sys **103.6–103.7 ms** p50 (m17: 323.6 / 127.3). Both sides are
*lower* in absolute terms — m18 is a quieter window — while the ratio reads slightly worse because the
sys side improved proportionally more. The m17 ratio came from a load 3–92 window whose own per-round
spread was 2.23–3.05×, so a 2.51 → 2.62 movement is inside that window's noise. No startup code
changed this session (nothing on the boot path was touched).

### 7.4 `binary_append`'s ratio improved 40.02× → 16.10× and is **not** attributable to this change
`binary_append` is deliberately absent from the A/B. The rust absolute improved (m17 4 220 897.5 ns/call
at load 3–92 → **1 314 000–1 333 000 ns/call** quiet) **and** so did the oracle's (108 253.0 →
81 800–82 370 ns/call), so part of the ratio movement is the quiet window on both sides. Do not credit
this session's change for it.

## 8. Reproduction

```sh
# build the driver and the runtime
erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench \
    --manifest-path erts_rust/Cargo.toml --offline

# the final matched rounds (alternate the side order round by round; log /proc/loadavg around every pass)
WL=baseline,slice,list_walk,copy_bound,foreign,gc_bound,binary_append
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust --workloads $WL \
    --out-file $TMPDIR/fin-rust-r8.json --verbose
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime sys  --workloads $WL \
    --out-file $TMPDIR/fin-sys-r8.json  --verbose
# r9 = sys then rust; r10 = rust then sys

# the binary A/B (build the pre-change tree once, then alternate round by round)
git archive 5ddf6e72e4 erts_rust | tar -x -C $TMPDIR/m18-base
CARGO_TARGET_DIR=$TMPDIR/m18-base-target erts_rust/scripts/cargo.sh build --release -p beam_rt \
    --manifest-path $TMPDIR/m18-base/erts_rust/Cargo.toml --offline
BEAM_RT=$TMPDIR/m18-base-target/release/beam_rt taskset -c 3 setsid -w \
    erts_rust/target/release/bench run --quick --runtime rust --workloads gc_bound,copy_bound \
    --out-file $TMPDIR/ab2-pre-r1.json
BEAM_RT=erts_rust/target/release/beam_rt              taskset -c 3 setsid -w \
    erts_rust/target/release/bench run --quick --runtime rust --workloads gc_bound,copy_bound \
    --out-file $TMPDIR/ab2-post-r1.json

# the external targets
setsid -w erts_rust/target/release/bench run --full --runtime sys \
    --workloads erlc_compiler,elixir_compiler,cowboy --out-file $TMPDIR/ext-full-quiet.json --verbose
```

`bench` honours `BEAM_RT`, `ERTS_ORACLE`, `ERLC`, `BEAM_RT_TIER`, `BENCH_*` and `--repo <id>=<path>`;
it never sets `ERTS_STDLIB_DIR`. **Archive `out/` copies into `erts_rust/tools/bench/results/` with
only the mechanical path normalisations — never edit a metric value.**

## 9. Crate-test counts for the crates this session touched

| crate | command | result |
|---|---|---|
| `beam_gc` | `-p beam_gc --no-fail-fast` | **377 passed / 0 failed / 2 ignored** (51 targets) |
| `beam_process` | `-p beam_process --no-fail-fast` | **654 passed / 0 failed / 1 ignored** (52 targets) |
| `beam_rt` | `-p beam_rt --lib` | **1 806 passed / 0 failed / 4 ignored** |
| `beam_rt` | `-p beam_rt --tests` (152 integration targets + 2 unit binaries) | **825 passed / 0 failed / 41 ignored** |
| workspace | `build --workspace --all-targets` | **warning-free** |

The ignores are all pre-existing opt-in suites (two-VM distribution, `selfhost_*`, `ref_id_space`,
`nif_resource_gc`, the documented non-yielding-runner `preemption` hang, two `offheap` doctests).
No test was deleted, `#[ignore]`d or weakened, and no library behaviour was changed to make a test pass.

## See Also
- `MEASURED.md` → the `beam_gc` change's own structural counters and the policy rule.
- `../beam_rt/src/runner/MEASURED.md` → the runner's per-slice counters on the new rule.
- `../../tools/bench/RESULTS.md` → the m17 record (both sides named, m17's own load band).
- `../../BOTTLENECKS.md` → the ranked bottleneck ledger this session updated B1 from.
- `results-m18/` → the raw `Report` JSON behind every figure above.
