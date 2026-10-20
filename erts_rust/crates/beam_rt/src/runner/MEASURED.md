# `beam_rt::runner` — measured workload record (slice charge · growth loops · collection cadence · off-heap reclamation)
The current-state measurement record behind [`CONTEXT.md`](CONTEXT.md) *Measured*, which carries the headline ratios and the structural pins.
This file carries every table, per-call counter, estimator and caveat: the per-workload speed ratios, the per-iteration collection/major rates and nursery geometry, the boundary door's ON-vs-OFF A/B, the slice budget's exact charge, the `foreign` mechanism, the off-heap prune's per-fixture envelope and the `emqx_base62:encode/1` end-to-end record.
Nothing here is a change log: a figure is evidence for the current tree, and the cited command or test re-derives it.

## Method discipline
- **A wall-clock figure is only as good as the host.** The bench host is shared and loaded, so a ratio read at a named `/proc/loadavg` band is the durable signal and the absolute wall time is not; re-run the cited command for a live number.
- **Prefer the deterministic counter.** Every `case_*_ops`/`case_*_calls`/`case_*_elements` count and every per-call counter-table row is contention-immune, so a *shape* survives a loaded host even where the absolute walls do not.
- **Never compare two trees measured in different processes on this host.** A ON-vs-OFF comparison is taken out of **one** build, as interleaved `A B A B …` legs, with the OFF side reached by a throwaway `#[cfg(test)]` early return that is removed with the measurement.
- **A per-workload speed and memory figure is measured, never modelled.** The full cross-runtime record is [`../../PERF.md`](../../PERF.md), its dated snapshots [`../../PERF_HISTORY.md`](../../PERF_HISTORY.md), the per-call dispatch/trace cuts [`../../TRACE_AND_PERF.md`](../../TRACE_AND_PERF.md), and the bench driver's own record [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md).
- **`rust`** always means this runtime's release `beam_rt` on its default baseline-JIT tier; **`sys`** the system OTP 29.1.1 / erts-17.1 oracle.
- **A wall figure this file no longer re-derives is labelled as such** rather than quietly carried forward (see *Figures explicitly NOT re-verified*).

## The two-process delivery path (`copy_bound`)
Measured 2026-10-17 with the committed driver, `bench run --quick --runtime all --workloads copy_bound`, `taskset -c 3`, 15 rounds with the two runtimes paired **inside** each round and the min taken over the rounds; host `/proc/loadavg` 1.0–3.2.

| case (`case_<name>_ns_per_op`) | rust | sys | rust/sys |
|---|---|---|---|
| `send_recv_plain` (delivery only) | 1 943 ns (median 2 046) | 666.5 ns (median 672.8) | **2.92×** (per-round median 3.01×, range 2.90–3.16×) |
| `send_recv` (+ the ETF codec in the worker) | 7 631 ns (median 7 837) | 1 493 ns (median 1 506) | **5.11×** (median 5.22×) |
| `ets_copy` | 2 367 ns (median 2 391) | 219.3 ns (median 222.9) | **10.8×** (median 10.7×) |
| `etf_roundtrip` | 1 449 ns (median 1 521) | 655.0 ns (median 660.0) | **2.21×** (median 2.30×) |

The loop count behind each marginal is the deterministic, contention-immune `case_*_ops` counter — rust **15 000** / **10 000** / **200 000** / **40 000**, sys **1 000 000** / **500 000** / **2 000 000** / **1 500 000** — so the shape of the ratios survives a loaded host even though the absolute walls do not.

## The `run` route's sleep accounting
`strace -f -e trace=clock_nanosleep -T` on the release binary, 2026-10-17:
- `beam_rt run tests/fixtures/pingpong.beam pingpong:go/1 200000` — 200 000 synchronous round trips, the same two-process shape — makes **0** `clock_nanosleep` calls.
- `recv_after:after20/0` (parked on `after 20`, nothing else to run) makes **1** (19.9 ms).
- `recv_after:after2000/0` makes **80** (75 clamped 25 ms poll slices plus 8.6 ms/0.7 ms remainders, ≈2.0 s in total).

Both halves are pinned by `tests/run_route_sleep.rs`, which skips loudly when `strace` is absent.

## Growth-shaped loops: the per-call work is FLAT in N (deterministic counters, never wall clock)
`the_churn_workloads_per_call_counters_are_flat_in_n` and `the_heap_growth_workloads_per_call_counters_are_flat_in_n` each drive their fixture at three (resp. five) increasing iteration counts and print a `counter_table`: N vs slices / yields / waits / collections / major / fallbacks / peak young-nursery geometry / debt boundaries, plus the derived per-call ratios — every geometry figure quoted here is re-derived by that line, so read it rather than this prose for the mature-space numbers.
The same `churn` shape is pinned at *three* counts by [`tests/boundary_cost.rs`](tests/boundary_cost.rs).

### `churn`, native (default) tier
- collections/call **7.669165 / 7.672033 / 7.672758**, `major`/call **1.256372 / 1.051295 / 1.012825**, `reductions`/call **1 808.98 / 1 809.71 / 1 809.84**, slices/call **0.452274 / 0.452455 / 0.452464** at N = 2 000 / 10 000 / 40 000.
- The **collection** rate is flat: per iteration (15 346 / 76 728 / 306 918 collections over 2 000 / 10 000 / 40 000 iterations) it reads **7.6730 / 7.6728 / 7.6730**, so the per-call figure only closes the fixed offset.
- The **majors are not flat**: they are exactly one per iteration plus a fixed 514 (2 514 = 2 000 + 514, 10 514 = 10 000 + 514, 40 514 = 40 000 + 514), so the per-iteration rate is 1.2570 / 1.0514 / 1.0129 and falls toward **1** from above — it does not converge on 1.5.
- Neither total is the closed form `collections == 7N − 1` / `major == 3N / 2`: the door pins a young-only `CollectionKind::Minor` collection and escalates to an unpinned `collect` only when the promotion cannot fit the mature space, the native tier charges its blocks per block (which sets the slice counts these rates are expressed against), and the BIF-result copy-out's learned hint grows the nursery **before** each copy (`RuntimeContext::copy_out_hint` + `hint_grow_gate`, `../context/calls.rs`) — one growth event retires a nursery space, i.e. adds one unit of collection debt per iteration, which is what sets this shape's collection schedule.
- Young capacity plateaus at **609** words, and the mature generation then sits at 1 598 words capacity / 1 508 used (the 233-word figure older prose quoted is the pre-growth floor, not the plateau).

### `heap_growth`, native (default) tier
- collections/call **1.001100 / 1.001200 / 1.001220**, slices/call **0.001300 / 0.001280 / 0.001260** at N = 10 000 / 25 000 / 50 000.
- The 401-word request cannot fit a fresh nursery, so the growth/collection door is reached on **every** iteration — the derived relation the assertion pins: `collections == N + yields`, exact on all five rows (**5 006 / 10 012 / 25 031 / 50 062 / 100 125** collections against **6 / 12 / 31 / 62 / 125** yields).
- Its 610-word space is its **mature** generation; the young capacity plateaus at **233** words.

### Both shapes
`peak_young_spaces == 1` at every N, so the retired-nursery chain the `collection_debt` trigger protects stays collapsed rather than N-dependent; `yields == slices − 1` with `waits == 0` and `debt_bounds == 0` throughout (neither loop ever parks, and neither leaves an un-settled collection debt at a boundary).
Neither loop's cadence is this node's to set: the door decides *whether* a collection may run (and the counters above show it declining nothing), the collector's post-collection nursery sizing decides *how often* — [`../context/IMPLEMENTATION.md`](../context/IMPLEMENTATION.md) *What the doors do NOT decide*.

## The reduction/yield-boundary door's own cost on the hot shapes — ON vs OFF out of ONE build
`collect_at_reds_boundary` (`../context/gc/reds_door.rs`, reached from the `Yield` arm only) fires on a **publishing** slice-end park when `REDS_BOUNDARY_MIN_REDS` reductions have elapsed since that process's last boundary check **and** the young heap is **seven eighths** used (or the collector's `collection_debt` is set).
The watermark was retuned from `1/2` to `7/8` after the churn shape was found to pay a real amplification; the C's own rule (`ERTS_VBUMP_ALLOC_REDS`) fires when the allocation area is **exhausted**, not when it is half used.

### The estimator and the regime
- The estimator is the **paired per-call difference** — every counter here is deterministic, so the difference is identical in every pair — with the shape's own call count `N + 1` as the denominator (`Δ` is ON − OFF).
- `door checks/call` counts the publishing `Yield` parks handed to the door; `door colls/call` those that became a collection; `door.unpublished` is **0** on every shape (no hot shape ends a slice with a live float-register prefix); `fallbacks/call` is 0 on both sides of every row.
- The default-tier sweep is the native baseline JIT, where a shape that reaches no **refusing** seam never pre-empts (so most rows show ≤1 slice for the whole run — the residual under `CONTEXT.md` *Known Issues / Handoffs*); the interpreter sweep (`BEAM_RT_TIER=interpreter`) is where a compute-bound shape really pre-empts every ~4 000-reduction budget, which is the only regime in which the door is reached often.

### The retune (`gc_bound` churn, default tier)
At `7/8` the door is **free on the shape that used to pay**, measured on this tree: the shape's per-iteration collection rate stays flat and its retired chain stays collapsed (one young space, **609**-word capacity peak, at N = 2 000 / 4 000 / 10 000) while the door fires **113 / 230 / 571** times out of **904 / 1 809 / 4 524** boundary checks — one in 8.00 / 7.87 / 7.92, i.e. 0.0565 / 0.0575 / 0.0571 firings per call and **0.74 %** of the run's own 15 346 / 30 691 / 76 728 collections.
At `1/2` it had measured **+0.048 collections/call (+0.69 %)** with `major`/call **+8.9 %** against its door-off build.
`1/1` (the C-exact "used ≥ capacity") buys nothing over `7/8` (3 instead of 11 firings at N = 500, the same counters) and is *not* taken: the nursery grows in place, so an exact `used == capacity` test is satisfied on consecutive boundaries and could chain majors where `7/8` leaves the growth seam room.

### Figures explicitly NOT re-verified
The door-ON vs door-OFF `Δ` and every door-off figure: the "identical totals" at 6.984032 / 6.996002 / 6.999200 collections and 1.497006 / 1.499250 / 1.499850 majors per call at N = 500 / 2 000 / 10 000, the `7N − 1` / `3N / 2` closed form, the door-off `2` spaces / `376` words, and the `1/2`-watermark deltas above.
Reproducing them needs a door-disabled build — a throwaway `#[cfg(test)]` early return inside `../context/gc/reds_door.rs` — which is outside the scope of the last re-derivation, and they were taken on an earlier tree state: N = 2 000 measures **7.669165** collections and **1.256372** majors per call at a **609**-word young space, and the closed form does not hold at all (the door pins a young-only `CollectionKind::Minor` first and escalates only when the promotion cannot fit the mature space, so it does not force a cadence fullsweep).
The figures that *are* current are the ON-side ones above, printed and pinned by `tests/boundary_cost.rs`.

### Shapes the watermark cannot move
- `reds_boundary` (interpreter tier) reports one collection per pre-emption (`yields == collections`) with `major` 1 at a **610**-word `heap_size` against the door-off 3 074 841 / 29 485 327 words at N = 2 000 / 20 000.
- `etf_roundtrip` (interpreter tier) goes 0 → 0.005 collections/call at a bounded nursery against the door-off 1 347 856 / 3 075 217 words over **10 / 11** retired spaces at N = 10 000 / 40 000.
- The no-heap-admission shape is **debt-driven** and identical at every watermark ≥ `1/2` (every check reports `collection_debt`, so the occupancy arm never decides it): one collection per pre-emption, at the loop's own per-iteration reduction price over the `REDS_BOUNDARY_MIN_REDS` interval — which is why that interval is retained unchanged, and why the rate moved when the call/BIF seams began to be **priced** (`tests/reds_door.rs`'s measured 17..=20 reds/iteration band, i.e. ~200–225 iterations per 4 000-reds slice, against ~14 reds/iteration before the pricing).
- The remaining shapes of the whole-shape sweep have `door checks/call` of 0 or ≤1 per run, so no gate ever decides them and the paired difference is **exactly 0**: `send_recv`/`send_recv_plain` on both tiers (the ping-pong parks through the **`Wait`** arm, never `Yield`, so no boundary is ever offered) and, on the default tier, `etf_roundtrip`, `list_walk`, `slice:fib/1` and `heap_growth` at 3 000 (each completes inside one slice or never pre-empts at all).
- **Below measurement resolution**: `ets_copy` on the default tier (0.0002 checks/call, **one** extra collection over 40 000 calls) and `heap_growth` on the default tier (at most two over 12 000, +0.000167/call) — neither moves `major`, and neither shape's capacity is larger with the door on at the wider N.
- `slice:fib/1` on the interpreter tier is the intended fix in miniature (1 boundary → 1 collection where the door-off side collects nothing), and `list_walk` on the interpreter tier sees 2.005 checks/call whose collections the shape's own door absorbs (Δ 0).

### Nursery resizing is a real effect of one collection
`heap_growth` on the interpreter tier peaks at **843** words with the door on vs **610** off (+38 %), since a boundary collection re-sizes the nursery and every later admission keeps that size.
`ets_copy` on the interpreter tier and `churn` on the default tier go the other way (the door's own collection collapses the retired chain the door-off side retains; `churn`'s peak young capacity is **609** words with the door on, and the `376` door-off figure is from the earlier A/B, not re-measured).

### Residual: what is still missing is a young-only collection that GROWS the mature space
`ets_copy` on the **interpreter** tier still showed `+0.000400` collections/call and `major`/call `+0.000600` (down from `+0.000800` at `1/2`) — 16 extra collections over 40 000 calls, ~3× the collection-rate change, attributed at the time to a firing boundary also collecting the **mature** generation.
The mechanism sentence is out of date: the door no longer calls the escalating `beam_process::Process::collect` as its action (`../context/gc/reds_door.rs:306` pins a young-only `Process::collect_kind(CollectionKind::Minor)` first and escalates to `collect` only when that minor refuses because the promotion cannot fit the mature space), so a cadence fullsweep comes only from a mature generation too small to take the promotion.
What is still missing is a young-only collection that **grows** the mature space instead of refusing: `beam_gc::AnyCollector::collect_minor` reports typed heap exhaustion when the mature space cannot absorb the whole nursery, so every refusal on these shapes escalates.
The handoff is `beam_gc::AnyCollector::collect_young(&mut self, roots: &mut [Eterm]) -> Result<Collection>` (never escalates; grows/reclaims the mature space instead of a fullsweep; reports `CollectionKind::Minor`), recorded in `../../M1_GAPS_GC.md` *GC / heap model*.
The pinned, current fact is `churn`'s **0.74 %** firing rate at one collapsed 609-word space, which is what the ratio above would have to move to matter.

### The pins
[`tests/boundary_cost.rs`](tests/boundary_cost.rs) (`the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape`, three measured counts N = 2 000 / 4 000 / 10 000): `door checks == yields` at every row, `door collections` inside the measured `checks / 9 ..= checks / 7` band (**113/904 = one in 8.00**, **230/1 809 = one in 7.87**, **571/4 524 = one in 7.92** — no `checks / 8` law holds, which is why the band, not an identity, is what is pinned), `door.unpublished == 0`, `checks/call` in `0.40..=0.50` (measured 0.4518 / 0.4521 / 0.4524), the shape's own totals as an **exact measured table** (15 346 / 2 514, 30 691 / 4 514, 76 728 / 10 514 — the closed form `7N − 1` / `3N / 2` is *not* what this tree runs), one young space and a capacity ≤ 610 words, so the amplification cannot come back silently.
[`tests/reds_door.rs`](tests/reds_door.rs) pins the no-admission shape's relations (`yields == collections`, the 17..=20 reds/iteration band, `minor == collections − major` = `(8, 88)` at N = 2 000 / 20 000, a converged 610-word `heap_size` and one young space).

## The slice budget's charge on the native tier (slice/pass cost)
A natively executed compiled block is charged at a deopt, a heap-admission point and each **priced** BIF/call seam; only a *refusing* seam can end its slice, because the per-instruction charge belongs to the interpreter's loop and what a native block's budget drains is `budget`'s `spend` (`call_bif`/`call_bif_in_machine` after the dispatch, `resolve_external_call`/`resolve_local_call` before the forward) plus the two `ensure_heap` doors and any deopt.
- The non-allocating BIF-and-call fixture prices exactly `reductions == 5N + 9` (5 units per iteration + a constant 9 for the entry/epilogue seams, at N = 0/10/100/400/700/798 — the N = 0 row is that constant alone) and above the budget it saturates at exactly **4 006** (`budget` 4 000 + the epilogue's measured 6) — it was **0** before the pricing.
- The allocating `heap_growth` shape charges `reductions == 5N + 5` and pre-empts into `slices` 3/13/26/101, `yields` 2/12/25/100 and `collections` 2 002/10 012/20 025/80 100 at N = 2 000/10 000/20 000/80 000 — a bounded **800** iterations per 4 000-unit slice (`collections == N + yields`: one admission collection per iteration plus one boundary collection per pre-emption).
- **Residual**: a non-allocating BIF-and-call-only loop still ends no slice of its own on the native tier — `spend` never refuses and is a silent no-op once the budget is gone, so the *single* `Yield` those rows read is the run's **epilogue** seam (a same-shape fixture with no epilogue ends **zero** slices: `slices=1 yields=0 reductions=4 000` at N = 8 000) and the boundary collection door stays unreachable for the loop. Only the memory half of that residual has closed: that one `Yield` does collect, so `heap_size` sits on the collector's floor (233) instead of tracking N.
- The interpreter-tier control on the same fixture at N = 2 000 reads 10 slices / 9 yields / 9 collections / 1 major / `reductions` 36 018 / `heap_size` 233.
- A **refusing** charge cannot be taken at a call seam today: the native tier publishes `machine.current_code` at **block** granularity (`crates/beam_engine/src/jit/baseline/runtime.rs:993`) and its call paths consult the seam before storing any position of their own (`jit/baseline/call.rs:210` vs `:232`; `runtime.rs:948`; `runtime.rs:818`), so a refusal parks at block entry and the resume re-executes the block prefix — measured with a draft `charge()?`: `-eval 'halt(0).'` and `run slice.beam` both exited 3 with `unsupported: opcode: run: process blocked (no message available)`.
- **Handoff (out of this crate)**: `beam_engine` must store the instruction address (`machine.set_current_code(func, pc)`) before consulting the seam at those three sites, after which a call seam may refuse and the third pin of [`tests/slice_charging.rs`](tests/slice_charging.rs) is the assertion that must change.

## The `foreign` per-call record, and the literal-area-tagging mechanism
A dated snapshot at the `loadavg` 1.0–2.5 band: `bench run --quick --runtime all --workloads foreign`, `taskset -c 3`, 8 rounds, both runtimes paired per round, min over the rounds.

| case (`case_<name>_ns_per_call`) | rust | sys | rust/sys |
|---|---|---|---|
| `bstr_to_lower` | 12 413 ns | 324.3 ns | 38.3× |
| `bstr_capitalize_token` | 15 255 ns | 317.2 ns | 48.1× |
| `emqx_base62_encode` | 10 512 ns | 206.3 ns | 51.0× |
| `emqx_base62_decode` | 10 111 ns | 282.5 ns | 35.8× |

The per-call cost of a build-a-binary loop is **not** this table's figure — there is no per-slice off-heap mirror — so the controlled A/B is [`../../PERF_HISTORY.md`](../../PERF_HISTORY.md) *The per-call cost is independent of the iteration count* (`BEFORE` `3aa42c0e41` → `AFTER` `72d778dcf1`: `emqx_base62_encode` **138 544.3 → 5 667.6 ns/call** at `loadavg` 18–89, `decode` 20 698.3 → 10 028.7).

The mechanism is the **literal-area tagging**: `Nursery::grow` retires the old space into `retired` and only `Nursery::collapse` — reached at a collection — clears it, so a runtime that never collects keeps every retired space's capacity resident (RSS grows per call) and pays the valve's per-word scanned test (`root_is_unscanned` → `outside_scanned`) over a longer chain on every admission — that test is `O(1)` for an address outside the aggregate `scanned_bounds` and `O(log #spaces)` otherwise (`Nursery::contains` reads its ordered `retired_regions` index, `beam_gc/src/nursery.rs:194-213`; it never walks `spaces()`).
`beam_loader`'s `tag_literal_area` (`crates/beam_loader/src/literal.rs`, the safe form of the C load-time `erts_set_literal_tag`) tags the decoded literal area, so the walk recognizes these modules' constant literal-area binary as an off-heap literal — not a container in a region the collector does not scan at all (`context/gc.rs::word_region` answers `None` for a literal, which no relocation touches) — so the valve never declines, the descent latch stays closed, and the retired chain stays collapsed instead of growing with the iteration count.
A constant payload a module *builds* would instead be a foreign container that forwards nothing scanned: also inert, so also collected.

## The off-heap reclamation end to end — `emqx_base62:encode/1` on the unmodified EMQX module
All legs run the **identical** `.beam` — the unmodified `emqx_base62.erl` plus a driver loop — release builds, this runtime's default execution tier, **21 interleaved rounds** each.
`PRE-FIX` is `dc63fd7f49`, the commit immediately before the reclamation change; the oracle is the system ERTS (`erl` on `PATH`, OTP 29.1.1 / erts-17.1) at its own loop count.
The estimator is the **startup-cancelled paired marginal**, `median(wall@N − wall@0)/N`, each round sampling the `N`-op and the `0`-op pass **back to back**; rust loops are `N` = **60 000**, the oracle's `N` = **3 000 000**.
The host is **shared and loaded** (`/proc/loadavg` 7–19 across the session), so the ratios here and the memory figures are the durable signal and the absolute wall times are not.
These are **measured** figures rather than ones a test asserts: the mechanism's in-repo pin is the per-fixture suite named at the end of this section.

| leg | ns/call | `erlang:memory(binary)` @N=1 000 | @N=10 000 | `VmHWM` @N=10 000 | `VmHWM` @N=600 000 |
|---|---:|---:|---:|---:|---:|
| `PRE-FIX` (`dc63fd7f49`) | 6 901.8 | 1 072 000 B | 10 720 000 B | 132.1 MB | 878.7 MB |
| `POST-FIX` (current tree) | 6 110.6 | 272 288 B | 315 168 B | 119.9 MB | 119.8 MB |
| system-ERTS oracle | 223.4 | ~22 000 B | ~21 000–32 000 B | 69.9–70.9 MB | — |

**The current effect, stated as such.** `erlang:memory(binary)` is flat in N and the process's `VmHWM` is flat in N: the 9 000 extra calls of the N = 1 000 → 10 000 step add **42 880 B** — a one-off heap growth, not a per-call trend — and `VmHWM` is 119.9 MB at N = 10 000 against 119.8 MB at N = 600 000, a 60× step.
**What the reclamation removed is exactly 1 072 B per call** — a 1 024-byte payload plus its 48-byte off-heap header — and that leak was perfectly linear (`1 072 000` → `10 720 000` over the 10× step), so a process doing 10 M encodes held ~10 GB and its `VmHWM` went 132.1 → 878.7 MB between N = 10 000 and N = 600 000.
**Throughput:** `POST/PRE` = **0.885** and the independent 9-round pass' **0.958** agree in direction, but the size is load-dominated, so only a **4–12 %** speed-up is claimed; `POST/oracle` is **27.4×** against `PRE/oracle`'s **30.9×**.
**This is a memory fix, not a throughput fix**: the leaked payloads were unreferenced and never traversed, so the leak cost no measurable time and there is **no throughput cliff at 878 MB**.
The per-fixture envelope — the N-step figures, the sawtooth ladder, the disabled-site A/B and the retained-payload half of [`../../tests/off_heap_reclaim.rs`](../../tests/off_heap_reclaim.rs) — lives in [`../../CONTEXT.md`](../../CONTEXT.md) *Known Issues / Intentional M1 gaps*'s off-heap bullet, which owns those figures.
The `emqx_base62_encode` row of the `foreign` table above is the `bench` driver's own 8-round minimum-of-rounds pass at its own loop count, so its `rust/sys` ratio is not a like-for-like pair with this table's interleaved paired marginal; read each with its own method paragraph.

## The off-heap prune's per-fixture envelope
The cadence is the **pre-emption** cadence: every slice the `Yield` arm ends prunes, so a build-a-binary loop whose payloads land in the machine's reservoir is bounded by the bin-vheap budget even when it never reaches a `receive` — measured on [`../../tests/off_heap_reclaim.rs`](../../tests/off_heap_reclaim.rs)'s direct-recursion shape: **4 144** bytes at N = 1 000 and **4 144** at N = 10 000 with 4 096-byte payloads, **65 584 / 65 584** at 65 536 B — one surviving payload, no growth in N.
**A fun-loop body is covered by the OTHER prune, not this one.** `lists:foldl/3`/`lists:foreach/2` is entered as a *nested* external call, and each application of the lambda is served by `RuntimeContext::apply_fun` → `run_label_blocking`'s own short-lived `Machine` (`../context/pump.rs`); when that machine drops, its reservoir is handed to the runtime's **escaped-payload store** (`Runtime::eval_off_heap`), never to the process's `Machine::refc_binaries` — so the park arm cannot bound it, and the nested-run epilogue does: `RuntimeContext::adopt_nested_payloads` publishes the finished machine's own root set and prunes that store against it (`Runtime::prune_off_heap`, `O(1)`-gated), releasing only the handles the run itself adopted.
Measured: **377 104** bytes at both N = 1 000 and N = 10 000 (4 096-byte payloads, ~91 surviving of the run's own out-of-scope entries) and **393 504** at both with 65 536-byte payloads, against a linear **4 144 000** → **41 440 000** before that epilogue pruned, with the process's own reservoir at **0** handles throughout and its `erlang:statistics(reductions)` delta **0** (it neither pre-empts nor parks).
What is measured, and where the figures live: `erlang:memory(binary)` (`binary_alloc`'s live bytes) and the process's RSS across a build-a-binary loop, against the oracle's — the measured envelope and its caveats are [`../../CONTEXT.md`](../../CONTEXT.md) *Known Issues / Intentional M1 gaps*'s off-heap bullet, whose suite is [`../../tests/off_heap_reclaim.rs`](../../tests/off_heap_reclaim.rs) (the bench driver's [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md) carries the loop workloads' RSS).

## Not like-for-like: a matched-N `ns_per_call` against a runtime that never collects
On an allocation-shaped loop, a runtime that never collects runs **zero** collections — the admission declines and the nursery grows instead, ≈150 words/call — whereas this one collects several times per call (**7.67** collections and **1.26** majors per call on the `churn` proxy), so the difference is the memory management this runtime performs and the unbounded heap the other retains, not dispatch or tier.
The 1.25×–1.60× band the allocation-shaped `foreign` rows once read here at matched N is a **dated** figure from before the per-slice off-heap mirror was removed (that removal's A/B reads `emqx_base62_decode` 20 698.3 → 10 028.7 ns/call — [`../../PERF.md`](../../PERF.md)), not a current ratio.

## See Also
- [`CONTEXT.md`](CONTEXT.md) → the node's contract: tier selection, the park hand-off, the slice boundary, the collection-site inventory, `## Measured` (headline ratios) and *Known Issues / Handoffs*.
- [`../../PERF.md`](../../PERF.md) · [`../../PERF_HISTORY.md`](../../PERF_HISTORY.md) · [`../../TRACE_AND_PERF.md`](../../TRACE_AND_PERF.md) → the crate's figures, dated snapshots and the per-call overhead harnesses.
- [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md) → the bench driver's record (raw artifacts in `tools/bench/results/`).
- [`../context/IMPLEMENTATION.md`](../context/IMPLEMENTATION.md) → what the collection doors do and do NOT decide.
