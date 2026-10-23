# `beam_rt` — Measured: the mature-generation strategy per *workload class*
## What this file is

The measurement record of the mature-generation strategy A/B — barrier-free **copying** vs mark-region
**`Immix`** — on the two workload classes that no other suite covers: **C3, messaging/copy-bound** (two
shapes: a two-process ping-pong and an ETS copy) and **C4, a real in-process compile**.
`sibling <../beam_gc/MEASURED.md>` measures the strategies on allocation-shaped micro-workloads and the
break-even rule; `tests/gc_immix_payload.rs` measures them on a single "runtime-shaped" payload workload
at nursery-policy scale. This file answers the question those two cannot: **how does each strategy's price
land on a shape that is not an allocation loop at all.**

Every figure below is printed by the run itself, with one source and one command:

```
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --release -p beam_rt \
    --test gc_strategy_classes -- --nocapture --test-threads=1
```

`BEAM_RT_GCH_OVERRIDE_ROUNDS=5` raises the round count; `BEAM_RT_GCH_OVERRIDE_N` overrides the calibrated N.
The harness's own module docs (`tests/gc_strategy_classes.rs`) are the authority on the protocol; this
file is the record of what it measured.

## The two sides

| side | what it is |
|---|---|
| **subject** | `beam_rt` at commit `d54ddf32c1bf68f43edb34a52e1a879fcf11a7ad` (the harness commit), release profile (`opt-level = 3`, thin LTO, `codegen-units = 1`, `panic = "abort"`, `debug = 1`), rustc 1.98.1 |
| **execution tier** | `BEAM_RT_TIER` unset ⇒ the default `JitMode::Auto` ⇒ the **native baseline tier**, compiled lazily per function (the interpreter was run as a control — see [the RSS anomaly](#the-send_recv-peak-rss-anomaly-resolved-as-a-reproducible-strategy-effect)) |
| **toolchain present** | the system OTP **29.1.1 / erts-17.1** `erl` + `erlc` (`support::require_oracle`'s gate) |

**C3 and C4 are self-comparison arms, not an oracle comparison.** No row here contrasts this runtime with
OTP. The system `erlc` has exactly one job: compiling the two committed `.erl` fixtures
(`tests/fixtures/gc_class_copy_bound.erl`, `tests/fixtures/gc_class_compile.erl`) into a per-class scratch
directory at test time, and the system `erl` only satisfies the harness's toolchain gate. Both arms are the
same binary, the same fixtures, the same N, the same `fullsweep_after`, the same runtime path.

## The method

### The arms: a one-word control at the rule's break-even

The runtime cannot pin a mature strategy — `ProcessTable::spawn` takes only a `beam_gc::HeapPolicy` — so the
strategy is a **consequence** of the configured mature generation, resolved by `beam_gc::select_mature`'s
automatic rule at `IMMIX_BREAK_EVEN_WORDS = 2 048` (8 × `BLOCK_WORDS` 256). The two arms therefore differ by
**one word** of configured mature space and by nothing else:

| arm | `HeapOptions::with_initial_mature_words` | the rule's verdict |
|---|---|---|
| `Copying` | 2 047 | `MatureKind::Copying` (`Automatic`) |
| `Immix` | 2 048 | `MatureKind::Immix` (`Automatic`) |

`fullsweep_after = 2` in **both** arms (two minors, then a fullsweep). That shared value is what makes the
arms differ by the strategy alone, and it is also what makes `major > 0` — the assertion that the mature
generation is exercised at all — reachable in a few-second run: the emulator default (65 535) would never
fullsweep here.

Nothing constructs a collector. Each arm is installed through `RuntimeConfig::with_heap_options` →
`Runtime::new` → `spawn_process`, i.e. the production path; `GenImmixCollector::with_config` is never used.

### The in-process `MatureKind` assertion, and what it protects against

Every measured run asserts, **inside the process it measured**, that `collector().kind()` is the arm's
expected `MatureKind`, that `collector().mature_choice()` reports `MatureSource::Automatic` at the arm's
configured word count, and that the mature space's reported capacity reflects that geometry. It protects
against exactly one failure, and it is the failure that would invalidate the whole record: **both arms
silently running the copying space** — a broken knob, or an ambient `BEAM_RT_GC_STRATEGY` (which
`Runtime::new` applies *after* the crate's own heap options). Such a run would make every difference below
vanish while every number still looked plausible. The harness therefore also refuses to run at all when
`BEAM_RT_GC_STRATEGY` is set in its environment, and removes it from every child's environment.

### One child per (class, arm, round), VmHWM polling, interleaved arms

The `#[test]`s are drivers; they measure nothing. Each runs its rounds with the arms **strictly interleaved**
(`Copying`, `Immix`, `Copying`, `Immix`, … — never arm-blocked, so a change in host load lands on both arms
alike), and every `(class, arm, round)` is measured by **one fresh child process**: the same test binary
re-executed as `--exact child_runs_one_arm_and_prints_a_row --ignored --nocapture --test-threads=1`, with
`BEAM_RT_GCH_{CLASS,ARM,N,ROUND,FIXTURE,OUT,SRC,MODULE}` selecting the work.

A child per arm because `Runtime::gc_stats()` is per-runtime and a collector's counters are cumulative, the
engine's block cache and the atom namespace are process-global, and a crash must be one *arm's* failure.

The child builds its own runtime, boots, loads the fixture, spawns the workload as a real scheduled process
and drives it with `run_one_slice` until it returns — sampling the live collector after every slice. The
parent polls the child's `/proc/<pid>/status` `VmHWM:` every 20 ms while it runs (the kernel's monotonic
high-water mark, so the max sample is the arm's true peak RSS), reads `/proc/loadavg` before and after every
round, and kills the child at `CHILD_DEADLINE` (900 s).

Per arm per round the parent records: the runtime's minor/full-sweep counts and the **measured process's own**
counts, `words_copied`, `objects_copied`, `words_reclaimed`, `mature_used_words`/`mature_capacity_words`, the
**collection-weighted mean nursery capacity** (`Σ kᵢ·Cᵢ / Σ kᵢ` over the slice samples, weighted by the
collections each capacity actually served), the peak current-space nursery, the observed
`MatureKind`/`MatureChoice`, every GC failure counter, the slice count, the wall time, the child's peak
`VmHWM` and the load before/after.

### The estimator, and why absolute wall times are not results

The host is shared and loaded (see [the host load](#the-host-load-during-the-measurement)). Every report
pairs each arm's round with the other arm's round from the *same* round and prints the **median paired
difference** (`Immix − Copying`) and the **median paired ratio** per metric, plus the observed spread of the
differences. No absolute wall figure below is a result; the ratios are. The deterministic counters need no
estimator at all — they are bit-identical round to round (see [the noise floor](#the-noise-floor)).

### The workloads

* **C3 `send_recv(N)`** — a real two-process ping-pong of a ~390-word payload (`{K, [{K,I,I+K} | …]}`, 64
  four-word tuples behind 64 cons cells), N round trips, so every round crosses the process copy boundary
  **twice**; each `receive` bounded by `after 60000`, the peer joined through a monitor, the last payload
  checked element by element by the child. N = 40 000.
* **C3 `ets_copy(N)`** — one table, one key, a fresh payload inserted and copied back out per round: the
  table-store copy boundary, never table growth. N = 150 000.
* **C4 `run(N)`** — one real `lib/stdlib/src/ordsets.erl` compiled N times **inside the measured process**
  (`no_spawn_compiler_process`, `deterministic`, `no_docs`, one `{outdir, …}` per compile), so the
  compiler's readers, passes and code emission allocate on the heap being measured. N = 8. Every artifact is
  re-loaded by the child (`verified=true` on every row below) and re-checked on disk.

## The noise floor

`the_noise_floor_of_one_arm_repeated` measures the copying arm on `send_recv` at the calibrated N, three
times, in six independent sessions (three full sessions plus three solo repeats) — 18 repeats:

| metric | spread across the repeats, per session |
|---|---|
| the deterministic counters (`own_collections` 60 000, `own_major` 20 000, `words_copied` 6 240 204, `mean_nursery` 376) | **1.000× in all six sessions** — bit-identical in all 18 repeats |
| peak `VmHWM` | 1.000×–1.002× (319 584–320 184 kB across all 18) |
| `wall_ms` | 1.003×–1.013× in five sessions; **1.614×** in one (2 302 / 2 332 / 3 715 ms, taken while the 1-min loadavg spiked to 7.04) |

**The band any claimed effect must beat: ~1.01–1.02× on a paired median ratio, and nothing at all on a
deterministic counter unless it is exact.** A single round can be stretched by up to 1.6× by host load, which
is precisely why the estimator is paired and median-based and why absolute wall times are not results.

## The host load during the measurement

1-minute `/proc/loadavg`, every sample the runs printed (65 samples: the three full sessions' per-round
`before`/`after` lines plus their class headers, and the hand-driven reverse-order, N-scaling and tier
controls): **minimum 1.60, median 3.73, maximum 7.63** on a 14-core host. Per session: session A 4.80 → 6.76
(7.04 at its noise floor), session B 5.19 → 7.63, session C 1.60 → 1.83, the reverse-order drive 3.54 → 3.73.

The raw per-round `before`/`after` lines are in the harness's output. The load drifted *within* sessions, which
is the drift the interleaved schedule and the paired estimator absorb: the deterministic counters are identical
between session C (load 1.6–1.8) and session B (load 5.2–7.6), so the classes are comparable across sessions.

## C3 — messaging / copy-bound

Three sessions: A (3 rounds), B (5 rounds), C (3 rounds). The table below is session B; the "A / C" column of
the reproducibility rows gives the other two sessions' median paired ratios.

### `send_recv` — a two-process ping-pong, N = 40 000

Medians over 5 interleaved rounds (session B); deterministic metrics are identical in every round.

| metric | `Copying` | `Immix` | median diff | median ratio |
|---|---|---|---|---|
| minor collections (measured process) | 40 000 | 40 000 | 0 | 1.000 |
| fullsweeps (measured process) | 20 000 | 39 999 | +19 999 | **2.000** |
| runtime-level collections / fullsweeps | 60 519 / 20 173 | 80 518 / 40 172 | +19 999 / +19 999 | 1.330 / 1.991 |
| `words_copied` | 6 240 204 | 9 120 060 | +2 879 856 | **1.462** |
| `objects_copied` | 2 080 068 | 3 040 020 | +959 952 | 1.462 |
| `words_reclaimed` | 20 720 003 | 75 838 105 | +55 118 102 | 3.660 |
| `mature_used_words` | 84 | 1 216 | +1 132 | **14.476** |
| `mature_capacity_words` | 2 047 | 2 048 | +1 | 1.000 |
| mean nursery capacity | 376 | 609 | +233 | 1.620 |
| peak nursery capacity | 610 | 610 | 0 | 1.000 |
| peak `VmHWM` | **319 988 kB** | **166 400 kB** | −153 444 | **0.520** |
| wall | 2 132.2 ms | 1 822.5 ms | −269.4 | **0.863** |

Wall ratios across the three sessions: **0.859 / 0.863 / 0.864**; peak-RSS ratios **0.521 / 0.520 / 0.520**.
Every deterministic figure above is identical in sessions A, B and C to the last digit.

The volatile per-round series (session B) — this is the **anti-order-effect** evidence used below:

| round | `Copying` wall / peak | `Immix` wall / peak |
|---|---|---|
| 1 | 1 980.3 ms / 320 064 kB | 1 967.9 ms / 166 172 kB |
| 2 | 2 132.2 ms / 319 844 kB | 1 822.5 ms / 166 400 kB |
| 3 | 2 242.4 ms / 320 180 kB | 1 991.2 ms / 166 080 kB |
| 4 | 2 263.8 ms / 319 988 kB | 1 793.3 ms / 166 600 kB |
| 5 | 1 972.8 ms / 319 868 kB | 1 703.4 ms / 166 544 kB |

### `ets_copy` — the table-store copy boundary, N = 150 000

| metric | `Copying` | `Immix` | median diff | median ratio |
|---|---|---|---|---|
| minor collections (measured process) | 103 479 | 100 436 | −3 043 | 0.971 |
| fullsweeps (measured process) | 51 739 | 54 782 | +3 043 | 1.059 |
| runtime-level collections / fullsweeps | 155 220 / 51 740 | 155 220 / 54 783 | 0 / +3 043 | 1.000 / 1.059 |
| `words_copied` | 12 740 029 | 12 864 874 | +124 845 | **1.010** |
| `objects_copied` | 4 265 215 | 4 316 110 | +50 895 | 1.012 |
| `words_reclaimed` | 136 497 170 | 192 689 068 | +56 191 898 | 1.412 |
| `mature_used_words` | 228 | 1 216 | +988 | 5.333 |
| `mature_capacity_words` (last sample) | 2 047 | 1 792 | −255 | 0.875 |
| mean nursery capacity | 1 406 | 1 039 | −367 | 0.739 |
| peak nursery capacity | 4 183 | 1 598 | −2 585 | 0.382 |
| peak `VmHWM` | 17 656 kB | 17 564 kB | +12 | **1.001** |
| wall | 2 073.8 ms | 2 291.0 ms | +244.5 | **1.119** |

Wall ratios across the three sessions: **1.123 / 1.119 / 1.124**; peak-RSS ratios **0.999 / 1.001 / 1.009**.
`mature_capacity_words` is the *last sample*: the mark-region space reports block-granular capacity, so it ends
at 7 × 256 = 1 792 words against the 2 048 configured (the arm's geometry is asserted through
`max_mature_capacity ≥ configured`, not through this sample).

### The `send_recv` peak-RSS anomaly: resolved as a reproducible strategy effect

The construction-time smoke run reported ≈319 968 kB (`Copying`) against ≈166 456 kB (`Immix`) — a ~2× peak-RSS
difference in the direction the counter *should* have reversed (`words_copied` is **higher** under Immix). Every
measurement above reproduces it, so the question was whether it is a strategy effect or the schedule's
Copying-first order. It is **not an order artifact**, and the evidence is fourfold:

1. **No first-round outlier in either arm.** Across the 11 measured rounds of the three sessions the peak is
   flat per arm: `Copying` 319 496–320 180 kB, `Immix` 165 840–166 852 kB. Round 1 is not special anywhere.
2. **Reversing the arm order does not move the gap.** Driving the child by hand (same protocol, same polling)
   in the order `Immix, Copying, Immix, Copying, …` × 4: immix 166 200 / 166 276 / 166 376 / 166 496 kB,
   copying 319 732 / 319 808 / 319 616 / 319 576 kB — the same separation, with Immix *first*.
3. **The execution tier does not move it.** On the interpreter tier (`BEAM_RT_TIER=interpreter`) the same
   class measures 319 616 kB (`Copying`) / 166 080 kB (`Immix`); every deterministic counter is unchanged.
4. **It scales linearly with N, and only on this shape.** `send_recv` at N = 1 000 / 5 000 / 20 000 / 40 000:
   copying 21 212 / 53 212 / 167 868 / 319 908 kB, immix 19 688 / 33 800 / 91 456 / 166 496 kB — slopes
   ≈**7.7 kB per round trip** under `Copying` and ≈**3.8 kB** under `Immix` above a common ~18–20 MB baseline.
   `ets_copy` shows no such growth at all: 17.4–17.9 MB under both arms over 150 000 round trips.

A forensic snapshot of the child's mappings at its peak RSS says *where* it sits, not *why*: the excess is in
anonymous mappings of ~1 MB each, fully resident — 230 of them under `Copying` (plus one 59.6 MB region)
against 123 under `Immix` (plus two 33.1 MB regions) at N = 40 000. ~1 MB is
`beam_alloc`'s multi-block-carrier size (`SMALLEST_MBC_SIZE_DEFAULT`), so the holder is a pool of allocator
carriers, not the mature space (which reports 84 vs 1 216 words here) and not the copied live data.

**Resolution: (i) a reproducible strategy effect.** The treatment difference between the arms is exactly the
resolved strategy (they differ by one configured word), the effect reproduces in every session, in reverse
order, on both tiers, and scales with the run length; it cannot be an artefact of the schedule, of a first run
or of the estimator. **What stays unattributed is the mediator**: no counter this harness reports explains the
sign (copying copies 0.68× the words, runs 0.75× the collections, has the same peak nursery, and holds 14×
*less* in its mature space), and naming which allocator pool retains the 1 MB carriers would take
instrumentation this task does not add.

## C4 — a real in-process compile (N = 8 × `lib/stdlib/src/ordsets.erl`)

Medians over 5 interleaved rounds (session B).

| metric | `Copying` | `Immix` | median diff | median ratio |
|---|---|---|---|---|
| minor collections (measured process) | 868 | 585 | −283 | 0.674 |
| fullsweeps (measured process) | 420 | 435 | +15 | 1.036 |
| runtime-level collections / fullsweeps | 1 472 / 476 | 1 188 / 499 | −284 / +23 | 0.807 / 1.048 |
| `words_copied` | 13 424 794 | 6 345 552 | −7 079 242 | **0.473** |
| `objects_copied` | 4 544 189 | 2 208 816 | −2 335 373 | 0.486 |
| `words_reclaimed` | 36 508 425 | 65 232 140 | +28 723 715 | 1.787 |
| `mature_used_words` | 2 934 | 118 256 | +115 322 | **40.305** |
| `mature_capacity_words` | 10 958 | 121 600 | +110 642 | **11.097** |
| mean nursery capacity | 40 877 | 47 812 | +6 935 | 1.170 |
| peak nursery capacity | 271 763 | 514 836 | +243 073 | 1.894 |
| peak `VmHWM` | 170 444 kB | 177 940 kB | +7 520 | **1.044** |
| wall | 1 762.9 ms | 2 019.3 ms | +256.3 | **1.145** |

Wall ratios across the three sessions: **1.200 / 1.145 / 1.152**; peak-RSS ratios **1.043 / 1.044 / 1.044**;
`words_copied` ratio 0.473 in all three. Per-round volatile series (session B): copying 1 758–1 764 ms /
170 076–170 696 kB, immix 2 016–2 024 ms / 177 788–178 004 kB.

The compile class is the one shape in which the mark-region space can pin: the compiler's live set is large and
long-lived across a pass, so the space relocates nothing and **copies 2.1× fewer words and 2.1× fewer objects**.
It pays for that in geometry and in time: its reported mature occupancy is the line-granular occupancy of the
space it pins (118 256 words) against the copying arm's promoted object words (2 934), a mature **capacity**
11.1× larger, and the run's peak RSS is **4.4 % higher** — while wall rises **14.5 %**.

## Verdicts

* **C3 `send_recv` — does not win copy work; wins time and footprint.** Immix moves **1.46× more** words and
  fullsweeps **2×** as often (its class-typical extra fullsweeps come from `GenImmixCollector` promoting a due
  minor to a major when the block space is tight, `beam_gc/src/immix_collector.rs:340`,
  `fullsweep_due(..) || tight`), yet finishes **13.7 % faster** (ratio 0.863) and peaks at **52 %** of the
  copying arm's RSS. So on this class the copy-work axis does not predict the wall axis: the mature live set is
  ~100 words, neither space can win by pinning it, and what the copying space pays per fullsweep is not the
  words it moves. The candidate mechanism — a copying fullsweep relocating into a fresh mature space where
  Immix marks and sweeps in place — is consistent with the direction but is **not** isolated by any counter
  here (see [unattributed](#what-is-unattributed)).
* **C3 `ets_copy` — does not win.** The copy volume is a wash (`words_copied` 1.010×, `objects_copied`
  1.012×) while the run is **11.9 % slower** (ratio 1.119) and the footprint is flat (RSS ratio 1.001): with no
  copy-work saving to buy, the mark-region space's marking and line accounting is pure cost. Its `5.3×`
  reported mature occupancy is 228 → 1 216 words — irrelevant in absolute terms because the payload lives in
  the table store, not on the process heap.
* **C4 compile — wins copy work by 2.1× while costing 11.1× mature capacity, 4.4 % peak RSS and 14.5 % wall.**
  It moves **0.473×** the words and **0.486×** the objects (the pin its geometry is designed for), and pays a
  line-granular footprint 40× the copying arm's reported occupancy, an 11.1× larger mature capacity, a 4.4 %
  higher peak RSS and a 14.5 % longer wall. Read with the sibling rule's caveat (the break-even is a
  **copy-work** break-even, not a wall-clock one), that is a **trade, not a win** on this host.
* **No class here is a win on all three axes.** Every class has at least one axis on which the copying default
  is the cheaper choice, which is consistent with — and does not contradict — `beam_gc`'s standing decision
  that `MatureKind::Copying` stays the default.

## The allocation product is not a cross-strategy invariant

`tests/gc_immix_payload.rs` relies on an accounting identity: `collections × mean nursery capacity` is the
young words a run's own heap doors admitted, and because both arms allocate the same terms through the same
live-set nursery rule, the product should match cross-strategy. **It does not hold on these three classes:**

| class | `Copying` | `Immix` | ratio |
|---|---|---|---|
| `send_recv` | 60 000 × 376 = 22 560 000 | 79 999 × 609 = 48 719 391 | **2.160×** |
| `ets_copy` | 155 218 × 1 406 = 218 236 508 | 155 218 × 1 039 = 161 271 502 | **0.739×** |
| `compile` | 1 288 × 40 877 = 52 649 576 | 1 020 × 47 812 = 48 768 240 | **0.926×** |

(All six products are bit-identical in sessions A, B and C. These reproduce the construction-time smoke
figures — 2.16×, 0.739×, 0.930× — to the reported precision.)

**The mechanism, both halves visible in the rows above:**

1. **The nursery is re-sized from a live set the two collectors report differently.** A fullsweep re-sizes the
   nursery from the **whole-heap** live set (`beam_gc/src/nursery.rs::collapse_after_fullsweep` →
   `HeapPolicy::young_after_fullsweep`, i.e. `2 × live + 64` through the growth table), and the mature
   generation's contribution to that live set is exactly what the strategies report in different units: the
   mark-region space reports **line-granular occupancy** (`mature_used` 1 216 words on both copy-bound shapes,
   118 256 on the compile) where the copying space reports the **promoted object words** (84 / 228 / 2 934). A
   different whole-heap live set gives a different nursery target — 376 vs 609 words on `send_recv`, 1 406 vs
   1 039 on `ets_copy`, 40 877 vs 47 812 on the compile.
2. **The two collectors do not share a fullsweep cadence.** The copying collector runs the policy's cadence
   exactly (`fullsweep_after = 2` ⇒ one major in three: 20 000 / 60 000 on `send_recv`, 51 739 / 155 218 on
   `ets_copy`, 420 / 1 288 on the compile — 1/3 in each case), while `GenImmixCollector` **promotes a due
   collection to a major when its block space is tight** (`beam_gc/src/immix_collector.rs:340`,
   `fullsweep_due(..) || tight`). On `send_recv` that lands almost exactly every other collection
   (39 999 / 79 999) and is what drives `own_collections` up by 1.33×; on the compile it adds a mild
   0.326 → 0.426 major share (435 / 1 020).

Both inputs to the product are therefore strategy-dependent, so the product is too — in both directions.

**What the harness does about it:** it **reports** the product and does not assert it —
`report_allocation_identity` prints every round's `copying c × m` / `immix c × m` / ratio, and
`IDENTITY_ASSERTED: &[]` is the (empty) list of classes on which the 1 % cross-arm assertion is enabled. The
only product assertion left is that both arms admitted *some* young words (a zero product means no nursery
admission at all). A cross-arm band on this product would be a number fitted to three shapes.

**What remains as the load-invariant evidence instead:** the noise floor's deterministic counters. Over 18
repeats in six sessions at loadavg 1.6–7.6, one arm reports the **same** `own_collections` (60 000),
`own_major` (20 000), `words_copied` (6 240 204), `objects_copied` (2 080 068), `words_reclaimed` (20 720 003),
`mature_used` (84), `mean_nursery` (376) and product (22 560 000) to the digit. The counters are a pure
function of the workload and the arm; only the wall time and the peak RSS move with the host. That
bit-identity — not a fitted band — is what makes a cross-arm difference in the counters meaningful.

## The default geometry: no class win is available there

**The runtime's default mature generation is 233 words** (`beam_gc::HeapPolicy::default().initial_mature_words`
= 233, one of the `erts_init_gc` growth-table entries). 233 < `BLOCK_WORDS` (256), and below one whole block
**Immix is strictly dominated** — it has no full block to pin, so it moves more words than a copy and costs a
multiple of the footprint. `select_mature`'s rule never selects it below `IMMIX_BREAK_EVEN_WORDS = 2 048`.

Two consequences, stated outright:

* **No class win measured here is available at the default geometry without a policy/selection-rule change.**
  A default-policy process runs the copying space whatever its workload is; Immix is reachable only from a
  configured mature generation ≥ 2 048 words or an explicit pin. The A/B's arms are *one word either side of
  that rule*, not a description of two available configurations.
* **No default and no selection rule was changed by this work.** The measurement added no arm, no knob and no
  branch: it ran the production path with two configured mature sizes and recorded what happened.

## What is unattributed

* **The mediator of the `send_recv` peak-RSS gap.** The *difference* is a reproducible strategy effect (the
  four lines of evidence above). Which pool retains the ~1 MB carrier-sized regions, and why the copying arm
  needs ~2× as many at the same allocation volume, is **not** established by this harness: every counter it
  reports moves in the opposite direction to the RSS, and the mapping snapshot names only the region size.
* **The wall-time mechanism on `send_recv`.** Immix is 13.7 % faster while moving 1.46× more words. The
  direction reproduces in every session and in reverse order, so it is not estimator noise, but "fewer
  relocations per fullsweep" is a plausible mechanism this record did **not** isolate (no counter here
  separates relocation from marking).
* **The absolute peak-RSS figures.** They are peak `VmHWM` of a child process that also compiles nothing, boots
  the 22 preloaded modules and hosts the runtime's own service processes; the ~18–20 MB baseline those
  contribute is measured (`ets_copy`'s flat 17.4–17.8 MB) but not subtracted anywhere above.
* **Pause.** The harness samples the collector at slice boundaries and does not stop the world for a pause
  measurement, so these classes say nothing about per-collection pause. `beam_gc`'s own pause figures are
  single-threaded, in-process and explicitly not reproducible (`../beam_gc/MEASURED.md`).

## Reproducing

```
# the whole record: three classes + the noise floor, 3 rounds each (≈47 s on a loaded 8–14 core host)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --release -p beam_rt \
    --test gc_strategy_classes -- --nocapture --test-threads=1

# a single class; more rounds
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --release -p beam_rt \
    --test gc_strategy_classes -- --exact c4_in_process_compile_under_both_mature_strategies \
    --nocapture --test-threads=1
BEAM_RT_GCH_OVERRIDE_ROUNDS=5 …      # rounds per class
BEAM_RT_GCH_OVERRIDE_N=2000 …        # every class's N (iteration on a loaded host)

# one arm by hand (the reverse-order control): the child protocol is the parent's own
BEAM_RT_GCH_CLASS=send_recv BEAM_RT_GCH_ARM=immix BEAM_RT_GCH_N=40000 BEAM_RT_GCH_ROUND=1 \
BEAM_RT_GCH_FIXTURE=<scratch>/gc_class_copy_bound.beam \
    <test-binary> --exact child_runs_one_arm_and_prints_a_row --ignored --nocapture --test-threads=1
```

The suite needs `/proc` (peak-`VmHWM` sampling), `erlc` (the fixtures) and `erl` (the toolchain gate); each
absence is a **loud skip**, never a silent pass. It refuses to run with `BEAM_RT_GC_STRATEGY` set, because an
ambient pin would collapse both arms into one strategy.
