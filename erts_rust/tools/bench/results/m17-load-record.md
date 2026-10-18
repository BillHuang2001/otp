# m17 host/load record (2026-10-18)
Session: **m17, 2026-10-18**, 14 logical cores, **shared host, loaded throughout**. `/proc/loadavg`
(1/5/15-min) was read immediately before and after every run and one benchmark process ran at a time.
The 1-min figure ranged **3.33 – 92.60** over the session and rarely sat below 10, so m17 is a
**loaded** session (m16's band was 3–76, m15's 2–5): absolute wall times and throughputs are
load-contaminated on both sides and are **not** comparable with earlier sessions' absolute figures.
Every matched round and every binary-A/B pass is pinned with `taskset -c 3`; the two single-side
external passes are left **unpinned** so they stay comparable with m15/m16's external rows.
Raw logs: `m17-xab-load.log`, `m17-ab-load.log`, `m17-ext-load.log`.
| run | artifact(s) | pin | `/proc/loadavg` (1-min) before → after |
|---|---|---|---|
| matched cross-runtime rounds 1–3 (rust/sys alternating) | `m17-xab-{rust,sys}-r{1,2,3}.json` | `taskset -c 3` | 5.14 → 65.76 (rising at every boundary) |
| matched cross-runtime rounds 4–6 (rust/sys alternating) | `m17-xab-{rust,sys}-r{4,5,6}.json` | `taskset -c 3` | 8.25 → 37.55 (a lull, then rising again) |
| binary A/B rounds 1–5 (new = HEAD, old = `a34ca61332`) | `m17-ab-{new,old}-r{1..5}.json` | `taskset -c 3` | 51.36 → 73.15 (per-run values below) |
| external targets, `--quick` | `m17-bench-external.json` | none | 64.13 → 53.02 |
| external targets, `--full` | `m17-bench-external-full.json` | none | 72.66 → 56.24 |
## The two load windows of the matched rounds
The first three rounds were taken while the host was filling up, so their own boundaries drift:
| round | rust pass before → after | sys pass before → after |
|---|---|---|
| 1 | 5.14 → 3.33 | 3.33 → 12.08 |
| 2 | 9.53 → 49.54 | 12.64 → 9.53 |
| 3 | 65.79 → 58.11 | 58.11 → 65.76 |
Rounds 4–6 were added after a relative lull appeared (8.25) and are kept **as separate archived
files** rather than replacing rounds 1–3, because the headline figures are the median of all six and
the per-round spread is the stability evidence.
| round | rust pass before → after | sys pass before → after |
|---|---|---|
| 4 | 8.25 → 28.13 | 28.13 → 33.58 |
| 5 | 18.84 → 42.46 | 33.58 → 18.84 |
| 6 | 42.46 → 51.40 | 51.40 → 37.55 |
The binary A/B ran after that, at a higher band:
`ab-r1` new 51.36→53.46 / old 53.46→66.35; `ab-r2` old 66.35→87.98 / new 87.98→92.60;
`ab-r3` new 92.60→68.16 / old 68.16→38.44; `ab-r4` new 41.95→33.93 / old 33.93→51.31;
`ab-r5` old 51.31→61.71 / new 61.71→73.15.
## What the load did to each side (the per-round stability evidence)
The sys oracle is comparatively load-insensitive; the rust runtime absorbs most of the variance. The
whole cross-runtime conclusion therefore rests on the **per-round ratio** (median of six), never on an
absolute pair of wall times.
| round | sys startup p50 | rust startup p50 | sys `list_walk` | rust `list_walk` | sys `ets_copy` | rust `ets_copy` |
|---|---|---|---|---|---|---|
| 1 | 103.5 ms | 289.2 ms | 2.28 ns | 22.57 ns | 230.6 ns | 2 191.7 ns |
| 2 | 125.1 ms | 317.7 ms | 2.31 ns | 31.89 ns | 243.7 ns | 2 312.2 ns |
| 3 | 136.8 ms | 339.1 ms | 2.72 ns | 29.68 ns | 312.3 ns | 2 385.2 ns |
| 4 | 147.5 ms | 329.4 ms | 2.66 ns | 28.88 ns | 267.7 ns | 2 636.3 ns |
| 5 | 124.9 ms | 305.8 ms | 2.38 ns | 31.41 ns | 291.0 ns | 2 640.3 ns |
| 6 | 129.4 ms | 395.1 ms | 2.75 ns | 34.85 ns | 318.5 ns | 2 882.6 ns |
The per-round ratios (`rust/sys`) stay inside a factor of ~1.4 of their median for every case:
startup p50 2.23–3.05 (median 2.51), `list_walk` 9.90–13.83 (median 11.80), `ets_copy` 7.64–9.85
(median 9.28). **No matched pass withheld a marginal** — all twelve `m17-xab-*` reports are 42/42
(workload × side) rows `ok` with no `marginal below timer resolution` note. The only withheld figures in
the session are three, all on the loaded `old` side of the binary A/B (`ab-old-r1`
`send_recv_plain`, `ab-old-r5` `emqx_base62_decode` and `churn`), whose raw notes are in those
artifacts.
## Why the external targets are host-starved (not regressed)
Both external passes ran unpinned at 1-min load 53–73, and the read-through is a host artefact, not a
driver or oracle change:
- `elixir_compiler`'s **compiler startup** reads **828.8 ms** (`--full`) / 601.3 ms (`--quick`) where
  m16 at load 3–9 recorded **288.1 ms** on the identical corpus and the identical installed `elixirc`
  — a 2.9× inflation from host contention alone;
- the `sys` `baseline` startup p50 in the same window is 127.3 ms against m16's 104.7 ms, the same
  direction and the same cause;
- `cowboy` throughput is bounded by `FULL_CONNECTIONS / per-request latency` (16 keep-alive connections
  over a 5 s `--full` window, 8 over 2 s in `--quick`, `src/workloads/cowboy.rs`), and under the same
  load the request distribution moved p50 0.070 → 0.211 ms and p99 0.827 → 3.03 ms (m16 at load 3–9:
  194 331 req/s; m17: 38 297 req/s) — the m17 row is a starvation reading of a server whose own
  build+start path measured normally, at the same request count per connection budget.
**Therefore m16's low-load external rows remain the reference for `cowboy`/`elixir_compiler`, and m17's
own external figures are the load-contaminated confirmation of the same shape.** No external figure in
this session is a regression claim; nothing in the external path changed between m16 and m17.
## Why the binary A/B is the only "changed since" instrument
Rounds 1–6 alternate the two sides minutes apart at a *drifting* load, so they support a **ratio**, not
a cross-session delta. The A/B (`m17-ab-*`) does: the same driver binary against HEAD and against the
m15-era runtime `a34ca61332`, alternating side order round by round, pinned to core 3, at 38–93 1-min
load. That is what attributes each move to code — e.g. `emqx_base62_encode` **0.260×**, `ets_copy`
0.927×, `list_walk` **1.215×** (the one regression) — while a raw wall-time diff across sessions would
attribute it to the host.
## Reproduction
Both helpers are pinned, record `/proc/loadavg` around every pass and use `setsid -w`:
```
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust \
  --workloads baseline,slice,list_walk,copy_bound,foreign,gc_bound,binary_append \
  --out-file erts_rust/tools/bench/out/m17-xab-rust-rN.json
```
(the sys pass is identical with `--runtime sys`; alternate the two per round, one process at a time),
and the A/B is the same command with `BEAM_RT` pointing at each `beam_rt` build in turn. The driver
spawns the runtime with an inherited environment, so `BEAM_RT`/`BEAM_RT_TIER` reach every child.
`RESULTS.md` §7 holds the authoritative command set; this file only records the *conditions*.
