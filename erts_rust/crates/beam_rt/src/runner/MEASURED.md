# `beam_rt::runner` — measured workload record (slice charge · growth loops · collection cadence · off-heap reclamation)
The current-state measurement record behind [`CONTEXT.md`](CONTEXT.md) *Measured*, which carries the headline ratios and the structural pins.
This file carries every table, per-call counter, estimator and caveat: the per-workload speed ratios, the per-iteration collection/major rates and nursery geometry, the boundary door's rationing, the slice budget's exact charge, the `foreign` mechanism, the off-heap prune's per-fixture envelope and the `emqx_base62:encode/1` end-to-end record.
Nothing here is a change log: a figure is evidence for the current tree, and the cited command or test re-derives it.

## Method discipline
- **A wall-clock figure is only as good as the host.** The bench host is shared and loaded, so a ratio read at a named `/proc/loadavg` band is the durable signal and the absolute wall time is not; re-run the cited command for a live number.
- **Prefer the deterministic counter.** Every `case_*_ops`/`case_*_calls`/`case_*_elements` count and every per-call counter-table row is contention-immune, so a *shape* survives a loaded host even where the absolute walls do not.
- **Never compare two trees measured in different processes on this host.** A ON-vs-OFF comparison is taken out of **one** build, as interleaved `A B A B …` legs, with the OFF side reached by a throwaway `#[cfg(test)]` early return that is removed with the measurement.
- **A per-workload speed and memory figure is measured, never modelled.** The full cross-runtime record is [`../../PERF.md`](../../PERF.md), its dated snapshots [`../../PERF_HISTORY.md`](../../PERF_HISTORY.md), the per-call dispatch/trace cuts [`../../TRACE_AND_PERF.md`](../../TRACE_AND_PERF.md), and the bench driver's own record [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md).
- **`rust`** always means this runtime's release `beam_rt` on its default baseline-JIT tier; **`sys`** the system OTP 29.1.1 / erts-17.1 oracle.
- **A figure this file does not measure is not claimed.** The one figure the counters here cannot reach — the boundary door's ON-vs-OFF `Δ` — is named as unclaimed (see *What is not claimed: the door's ON-vs-OFF `Δ`*) instead of being carried forward as a number.

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
- collections/call **2.362819 / 2.375462 / 2.376366**, `major`/call **0.162419 / 0.161684 / 0.161621**, `reductions`/call **1 808.98 / 1 809.71 / 1 809.84**, slices/call **0.452274 / 0.452455 / 0.452464** at N = 2 000 / 10 000 / 40 000.
- The per-call collection rate is **flat**: 4 728 / 23 757 / 95 057 collections and 325 / 1 617 / 6 465 majors over the 2 001 / 10 001 / 40 001 calls, so both totals are linear in N at a fixed per-call rate — the shape's live set is constant and so is the capacity the nursery rule installs from it.
- Neither total is a closed form: the door pins a young-only `CollectionKind::Minor` collection and escalates to an unpinned `collect` only when the promotion cannot fit the mature space, and the native tier charges its blocks per block (which sets the slice and pre-emption counts these rates are expressed against).
- The young nursery holds **2 585** words of capacity in **one** space at every N (peak, boundary-sampled), 1 551 of them used, and the mature generation holds **6 772** words of capacity with **6 712** used at N = 2 000 / 10 000 / 40 000.

### `heap_growth`, native (default) tier
- collections/call **0.500100 / 0.500050 / 0.500020 / 0.500010 / 0.500005**, slices/call **0.001400 / 0.001300 / 0.001280 / 0.001260 / 0.001260** at N = 5 000 / 10 000 / 25 000 / 50 000 / 100 000.
- The 401-word request fits the settled nursery twice and not three times, so the admission door collects once per two iterations (plus the one the run takes before its first iteration) — the derived relation the assertion pins: `collections == 1 + N / K` with `K == 2`, exact on all five rows (**2 501 / 5 001 / 12 501 / 25 001 / 50 001** collections against **6 / 12 / 31 / 62 / 125** yields).
- `major` is **0** on every row: each of those collections is a minor whose promotion (the live set at that admission point is empty) fits the mature region, so nothing escalates to a fullsweep.
- Its young nursery settles at **843** words (`2 x 401 = 802` of them, which is what makes `K == 2`) and its mature generation stays at the spawn policy's initial **233** words of capacity (`emulator_default_policy().initial_mature_words`) with **0** of them used at every row — only a fullsweep re-sizes that region, and none runs.

### Both shapes
`peak_young_spaces == 1` at every N, so the retired-nursery chain the `collection_debt` trigger protects stays collapsed rather than N-dependent; `yields == slices − 1` with `waits == 0` and `debt_bounds == 0` throughout (neither loop ever parks, and neither leaves an un-settled collection debt at a boundary).
Neither loop's cadence is this node's to set: the door decides *whether* a collection may run (and the counters above show it declining nothing), the collector's post-collection nursery sizing decides *how often* — [`../context/IMPLEMENTATION.md`](../context/IMPLEMENTATION.md) *What the doors do NOT decide*.

## The reduction/yield-boundary door's rationing on the hot shapes
`collect_at_reds_boundary` (`../context/gc/reds_door.rs`, reached from the `Yield` arm only) fires on a **publishing** slice-end park when `REDS_BOUNDARY_MIN_REDS` reductions have elapsed since that process's last boundary check **and** the young heap is **seven eighths** used (or the collector's `collection_debt` is set).
The watermark is seven eighths because the C's own rule (`ERTS_VBUMP_ALLOC_REDS`) fires when the allocation area is **exhausted**, not when it is half used, while `7/8` still leaves the growth seam room between two checks.

### The estimator and the regime
- Every counter here is a **deterministic count** (the runner's own tallies plus the collector's geometry), so a repeated leg reads byte-identically; the shape's own call count `N + 1` is the denominator of each per-call figure.
- `door checks/call` counts the publishing `Yield` parks handed to the door; `door colls/call` those that became a collection; `door.unpublished` is **0** on every shape (no hot shape ends a slice with a live float-register prefix); `fallbacks/call` is 0.
- The default-tier sweep is the native baseline JIT, where a shape that reaches no **refusing** seam never pre-empts (so most rows show ≤1 slice for the whole run — the residual under `CONTEXT.md` *Known Issues / Handoffs*); the interpreter sweep (`BEAM_RT_TIER=interpreter`) is where a compute-bound shape really pre-empts every ~4 000-reduction budget, which is the only regime in which the door is reached often.

### `gc_bound` churn at the seven-eighths watermark (default tier)
At N = 2 000 / 4 000 / 10 000 the shape's retired chain stays collapsed (one young space at a **2 585**-word capacity peak, 1 551 words of it used) while the door fires **105 / 210 / 532** times out of the **904 / 1 809 / 4 524** boundary checks handed to it — one in 8.61 / 8.61 / 8.50, i.e. 0.0525 / 0.0525 / 0.0532 firings per call and **2.22 / 2.22 / 2.24 %** of the run's own 4 728 / 9 479 / 23 757 collections.
The ration *is* the nursery's geometry: a boundary collection leaves the checks after it below the seven-eighths watermark, and the constant 2 585-word capacity makes the allocation each slice performs fill it at the same rate at every N — which is why the share is **constant** rather than rising, and why the assertion is the measured `checks / 9 ..= checks / 8` band it sits inside (4.6 / 9 / 29.3 boundary checks of slack to the lower edge, 8 / 16.1 / 33.5 to the upper).

### What is not claimed: the door's ON-vs-OFF `Δ`
Establishing that the door's collections *substitute for* the heap-admission door's rather than adding to them needs a door-disabled build — a throwaway `#[cfg(test)]` early return inside `../context/gc/reds_door.rs` — so **no `Δ` is claimed in this file**.
What the counters do pin is the rationing itself (one firing in 8.50–8.61 boundaries, a constant 2.2 % share of the run's own collections), a collapsed retired-space chain (one young space of 2 585 words at every N) and the exact per-row totals of [`tests/boundary_cost.rs`](tests/boundary_cost.rs)'s `MEASURED` table, which are off any closed form for three structural reasons: the door pins a young-only `Process::collect_kind(CollectionKind::Minor)` first and escalates to the unpinned `Process::collect` only when the promotion cannot fit the mature space (so a boundary firing does not force a fullsweep of its own), the native baseline tier charges its blocks per block (which sets the slice and pre-emption counts the per-call rates are expressed against), and the BIF-result copy-out's learned hint grows the nursery **before** each copy (`RuntimeContext::copy_out_hint` + `hint_grow_gate`, `../context/calls.rs`).

### Shapes the watermark cannot move
- `reds_boundary` (interpreter tier) reports one collection per pre-emption (`yields == collections`) with `major` 1 / 3 / 23 at N = 2 000 / 20 000 / 200 000 (9 / 89 / 899 collections) and a flat `heap_size` of **987** words at all three — `young_target_words(401) = next_words(866)`, the **fullsweep** rule's target for the one 400-element tuple the boundary window holds: the first boundary's kind-pinned minor cannot be promoted into the 233-word initial mature region, so it escalates and the fullsweep's `young_after_fullsweep` installs this target, which every later minor keeps inside `[young_minor_target_words(401) = 610, 8 x 610 = 4 880]`. The figure is a function of the iteration's footprint and never of the loop count.
- The no-heap-admission shape is **debt-driven**: every check reports `collection_debt`, so the occupancy arm never decides it and one collection per pre-emption follows from the loop's per-iteration reduction price over the `REDS_BOUNDARY_MIN_REDS` interval — the measured 17.8 / 18.0 reds per iteration (`tests/reds_door.rs`), i.e. ~220 iterations per 4 000-reduction slice.
- The remaining shapes of the whole-shape sweep have `door checks/call` of 0 or ≤1 per run, so no gate ever decides them and the door contributes no collection there: `send_recv`/`send_recv_plain` on both tiers (the ping-pong parks through the **`Wait`** arm, never `Yield`, so no boundary is ever offered) and, on the default tier, `etf_roundtrip`, `list_walk` and `slice:fib/1` (each completes inside one slice or never pre-empts at all).
- **Below measurement resolution**: `heap_growth` on the default tier ends only 6 / 12 / 31 / 62 / 125 slices at a `Yield` arm over **2 501 / 5 001 / 12 501 / 25 001 / 50 001** collections, so the boundaries the door is offered there are at most 125 against 50 001 collections.

### Nursery resizing is a real effect of one collection
`heap_growth`'s young nursery peaks at **843** words on the default tier, of which `2 x 401 = 802` are the two iterations its `K == 2` cadence carries.
`churn`'s peak young capacity is **2 585** words at every N in a single space: the shape's live figure is **201 words** (the collector's mean copy volume per collection, 945 124 / 4 724, read by `live_words` in `tests/boundary_cost.rs`), `young_minor_target_words(201)` is `next_words(201 + 64) = 376`, and a kept capacity stays inside `[376, 8 x 376 = 3 008]`. The growth seam steps one growth-table entry at a time and 2 586 is the last entry below that upper edge, so the seam has nothing left to step to; the settled capacity is that entry less the one word its block header accounts for, **2 585**.

### Residual: what is still missing is a young-only collection that GROWS the mature space
The door's action is a young-only `Process::collect_kind(CollectionKind::Minor)` (`../context/gc/reds_door.rs:317`) and it escalates to the unpinned `Process::collect` only when that minor refuses because the promotion cannot fit the mature space, so a cadence fullsweep comes only from a mature generation too small to take the promotion.
What is still missing is a young-only collection that **grows** the mature space instead of refusing: `beam_gc::AnyCollector::collect_minor` reports typed heap exhaustion when the mature space cannot absorb the whole nursery, so every refusal on these shapes escalates.
The handoff is `beam_gc::AnyCollector::collect_young(&mut self, roots: &mut [Eterm]) -> Result<Collection>` (never escalates; grows/reclaims the mature space instead of a fullsweep; reports `CollectionKind::Minor`), recorded in `../../M1_GAPS_GC.md` *GC / heap model*.
The pinned, current fact is `churn`'s **2.22 / 2.22 / 2.24 %** share of the run's own collections at one collapsed 2 585-word space.

### The pins
[`tests/boundary_cost.rs`](tests/boundary_cost.rs) (`the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape`, three measured counts N = 2 000 / 4 000 / 10 000): `door checks == yields` at every row, `door collections` inside the measured `checks / 9 ..= checks / 7` band (**128/904 = one in 7.06**, **258/1 809 = one in 7.01**, **646/4 524 = one in 7.00** — the band's lower edge, which is why the band, not an identity, is what is pinned), `door.unpublished == 0`, `checks/call` in `0.40..=0.50` (measured 0.4518 / 0.4521 / 0.4524), the shape's own totals as an **exact measured table** (3 468 / 169, 6 939 / 337, 17 341 / 838 collections and majors), one young space on a capacity that must not move with N (**1 220** words, inside the band *derived* from that row's own reported live figure of 158 words: `nursery_floor_words() = 90 <= 1 220` and `young_minor_target_words(158) = next_words(222) = 233 <= 1 220 <= YOUNG_SHRINK_FACTOR (8) x 233 = 1 864`, so the plateau is a fixed point of `young_after_collection`'s keeping branch and not a literal), and a mature capacity that must not move either (**4 185** words, the fixed point of `mature_target_words` = `MATURE_TARGET_LIVE_MULTIPLE` (2) x its post-fullsweep need).
[`tests/reds_door.rs`](tests/reds_door.rs) pins the no-admission shape's relations (`yields == collections`, the measured 17.8–18.0 reds/iteration band, `minor == collections − major` = `(8, 88)` at N = 2 000 / 20 000, one young space and a converged `heap_size` of **610** words = `young_minor_target_words(401)` at both rows).

## The slice budget's charge on the native tier (slice/pass cost)
A natively executed compiled block is charged at a deopt, a heap-admission point and each **priced** BIF/call seam; only a *refusing* seam can end its slice, because the per-instruction charge belongs to the interpreter's loop and what a native block's budget drains is `budget`'s `spend` (`call_bif`/`call_bif_in_machine` after the dispatch, `resolve_external_call`/`resolve_local_call` before the forward) plus the two `ensure_heap` doors and any deopt.
- The non-allocating BIF-and-call fixture prices exactly `reductions == 5N + 9` (5 units per iteration + a constant 9 for the entry/epilogue seams, at N = 0/10/100/400/700/798 — the N = 0 row is that constant alone) and above the budget it saturates at exactly **4 006** (`budget` 4 000 + the epilogue's measured 6).
- The allocating `heap_growth` shape charges `reductions == 5N + 5` and pre-empts into `slices` 3/13/26/101, `yields` 2/12/25/100 and `collections` 1 001/5 001/10 001/40 001 at N = 2 000/10 000/20 000/80 000 — a bounded **800** iterations per 4 000-unit slice, with `collections == 1 + N/K` and `K == 2` iterations carried by its settled 843-word nursery (one admission collection per two iterations plus the one before the first; a slice prices 800 iterations, a multiple of `K`, so the boundary door's firings land on cycle boundaries and add nothing).
- **Residual**: a non-allocating BIF-and-call-only loop still ends no slice of its own on the native tier — `spend` never refuses and is a silent no-op once the budget is gone, so the *single* `Yield` those rows read is the run's **epilogue** seam (a same-shape fixture with no epilogue ends **zero** slices: `slices=1 yields=0 reductions=4 000` at N = 8 000) and the boundary collection door stays unreachable for the loop; the one `Yield` it does take collects, so `heap_size` sits on the collector's floor (233) rather than tracking N.
- The interpreter-tier control on the same fixture at N = 2 000 reads 10 slices / 9 yields / 9 collections / 1 major / `reductions` 36 018 / `heap_size` **233** — `young_minor_target_words(0)`, i.e. the policy's nursery floor, which is the live-set-targeted minor rule's answer for the handful of words this shape keeps alive rather than a quantity that follows N.
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
On an allocation-shaped loop, a runtime that never collects runs **zero** collections — the admission declines and the nursery grows instead — whereas this one collects **3 468** times and majors **169** over the same 2 001-call `churn` run (**1.7331** collections and **0.0845** majors per call, at the same per-call rate at N = 40 000: 69 352 / 3 347 over 40 001 calls = **1.7338** / **0.0837**), so the difference is the memory management this runtime performs and the unbounded heap the other retains, not dispatch or tier.
The 1.25×–1.60× band the allocation-shaped `foreign` rows once read here at matched N is a **dated** figure from before the per-slice off-heap mirror was removed (that removal's A/B reads `emqx_base62_decode` 20 698.3 → 10 028.7 ns/call — [`../../PERF.md`](../../PERF.md)), not a current ratio.

## See Also
- [`CONTEXT.md`](CONTEXT.md) → the node's contract: tier selection, the park hand-off, the slice boundary, the collection-site inventory, `## Measured` (headline ratios) and *Known Issues / Handoffs*.
- [`../../PERF.md`](../../PERF.md) · [`../../PERF_HISTORY.md`](../../PERF_HISTORY.md) · [`../../TRACE_AND_PERF.md`](../../TRACE_AND_PERF.md) → the crate's figures, dated snapshots and the per-call overhead harnesses.
- [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md) → the bench driver's record (raw artifacts in `tools/bench/results/`).
- [`../context/IMPLEMENTATION.md`](../context/IMPLEMENTATION.md) → what the collection doors do and do NOT decide.
