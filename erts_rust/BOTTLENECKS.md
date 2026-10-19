# Measured performance bottlenecks — Rust ERTS (`beam_rt`)

What is still slow, by how much, **why** (mechanism + `file:line`), the structural counter that proves
the mechanism, **what has already been tried and rejected**, and the next concrete step.
Written so a later round can pick a lever without re-measuring a dead end — see §5 for the
consolidated rejected list before proposing anything.

Every figure names **both sides**: `sys` = the system ERTS (`erl` on `PATH`, **Erlang/OTP 29.1.1 /
erts-17.1**), `rust` = `erts_rust/target/release/beam_rt` at its **default baseline-JIT tier**.
Where a measurement is a **within-`beam_rt` A/B** (two rust builds) or a **within-crate** figure, the
entry says so explicitly — those are not cross-runtime ratios.

---

## 0. Which record is current (read this before quoting a number)

Three records exist and they are **not interchangeable**. Two of them measure the same rows under
very different host load, which is why the same workload can appear at two ratios.

| record | where | when | 1-min load | authoritative for |
|---|---|---|---|---|
| **Q** (quiet) | `crates/beam_engine/MEASURED.md` §*Cross-runtime record and the binary A/B* | 2026-10-20 | **1.02–2.37** | **the current rust-vs-sys ratios** for `list_walk`, `etf_roundtrip`, `ets_copy`, `send_recv`, `send_recv_plain`, `churn`, all four `foreign/*`; and the code-change A/B |
| **m17** | `tools/bench/RESULTS.md` (session `m17`, 2026-10-18) | 2026-10-18 | 3 → 92 | start-up p50/p99, `slice`, the memory rows, `binary_append`'s last **full-record** figure, the external targets |
| **fresh** | measured by hand while writing this file (§1.3), **no `out/` copy archived** | now | 161 → 181 | only what it says: one `binary_append` single round on a heavily loaded host — a hand measurement, not a record |

**Q supersedes m17 for the rows it measures.** Concrete examples of the difference, all
`rust`/`sys`:

| row | m17 (loaded) | **Q (quiet, current)** |
|---|---|---|
| `list_walk` | 11.80× | **7.72×** |
| `gc_bound/churn` | 18.75× | **14.77×** |
| `foreign/bstr_to_lower` | 18.81× | **9.42×** |
| `foreign/bstr_capitalize_token` | 22.46× | **12.65×** |
| `foreign/emqx_base62_encode` | 20.04× | **15.79×** |
| `foreign/emqx_base62_decode` | 25.88× | **19.00×** |
| `copy_bound/ets_copy` | 9.28× | **9.82×** |
| `copy_bound/send_recv` | 5.39× | **5.44×** |
| `copy_bound/send_recv_plain` | 3.16× | **2.91×** |
| `copy_bound/etf_roundtrip` | 2.21× | **2.16×** |

The direction of the difference is exactly what §1 predicts: **the sys side is comparatively
load-insensitive and the rust side absorbs the variance**, so a loaded window inflates the ratio.
Quote Q for "where do we stand"; quote the A/B for "did this change help".

---

## 1. Measurement method (reproduce it; do not misread it)

### 1.1 Two instruments, never chained

**(a) Matched cross-runtime rounds** — alternate a rust-only pass and a sys-only pass, one process at
a time, pinned, so each workload's two sides are sampled minutes apart; report the **median of the
per-round ratios**.

```sh
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust --workloads $WL \
    --out-file $TMPDIR/x-rust.json --verbose
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime sys  --workloads $WL \
    --out-file $TMPDIR/x-sys.json  --verbose
```

**(b) Binary A/B** — the **same `bench` binary** driving **two `beam_rt` builds** selected through
`$BEAM_RT`, five rounds per side, **alternating order round by round**. This is the only valid
"changed since" instrument.

```sh
BEAM_RT=$TMPDIR/old-target/release/beam_rt taskset -c 3 setsid -w erts_rust/target/release/bench run \
    --quick --runtime rust --workloads $WL --out-file $TMPDIR/ab-old-r1.json --verbose
BEAM_RT=erts_rust/target/release/beam_rt         taskset -c 3 setsid -w erts_rust/target/release/bench run \
    --quick --runtime rust --workloads $WL --out-file $TMPDIR/ab-new-r1.json --verbose
```

**Why they must not be chained:** (a) samples both sides minutes apart at a drifting load, so it
supports a *ratio within one window*; (b) alternates two builds of the same side, so it supports a
*delta*. Multiplying a ratio by an A/B factor composes two medians from different load windows, one
of them rust-only — meaningless. Also banned as a headline source: the runtime-major ordering
(`--runtime all`, all `sys` rows then all `rust` rows) — m16 measured **up to 18 % ordering bias** on
`list_walk`; and comparing raw `wall_ms` across two sides with different `N`.

Per-unit figures are **paired, startup-cancelled marginals**: each round runs the `N`-unit pass and
then the `0`-unit pass **back to back** and takes the **median of the per-round differences**
(`tools/bench/src/marginal.rs`), withheld below `3 × max(1 ms, 1.4826·MAD)`.

### 1.2 The shared host

14 logical cores, shared, no `/usr/bin/time` (the harness uses `date +%s%N` plus `VmHWM`/`VmRSS`).
**Record `/proc/loadavg` before and after every pass.** The window matters more than the workload:
Q ran at load 1–2.4, m17 at 3–92, and the single round in §1.3 at 161–181.
Consequence: **read the ratios and the deterministic `case_*_ops/calls/elements` counters, never a
single absolute wall time as a constant**, and never compare absolute times across sessions.

Deterministic companions (m17, sys / rust): `etf_roundtrip_ops` 1 500 000 / 500 000,
`ets_copy_ops` 2 000 000 / 250 000, `send_recv_ops` 500 000 / 90 000, `send_recv_plain_ops`
1 000 000 / 300 000, `churn_ops` 200 000 / 10 000, `list_walk_elements` 100 000 000 / 50 000 000,
`foreign` calls 1 500 000/50 000 · 1 500 000/40 000 · 3 000 000/60 000 · 3 000 000/60 000,
`binary_append_calls` 50 000 / 2 000 (`binary_append_bytes` 3 276 800 000 / 131 072 000).

### 1.3 The one figure measured while writing this file

`binary_append` is not in Q's row set, so its last full-record ratio is m17's (taken at load 3–92,
before the writable-append work landed). One substitute round was run here — **one rust leg and one
sys leg, sequentially, `taskset -c 3`, `setsid -w`**, load **161.53 → 170.00** (rust leg) and
**170.00 → 181.24** (sys leg):

| side | `ns_per_call` | `N` | bytes appended | paired median | noise |
|---|---|---|---|---|---|
| rust | **2 547 595.26 ns** | 2 000 | 131 072 000 | 5 095.191 ms | ±960.602 ms |
| sys | **140 649.11 ns** | 50 000 | 3 276 800 000 | 7 032.456 ms | ±151.761 ms |

⇒ **≈18.1×** on this host, at load ~170. Treat it as a *single loaded round*, not a settled ratio: it
is 1.9× the rust figure the quiet A/B records (1 333 404 ns/call), which is the load effect, and the
driver's own note says the append path measured is the real binary-append path (16 384 four-byte
segments into one writable payload), not a series of copies.
**No `out/` copy of this round was archived** — it was measured by hand while this file was written,
so it is a hand measurement, **not a record**, and it must be re-run through the driver (and archived
under `tools/bench/results/`) before it is quoted as a ratio.
A matched-round re-run is the way to publish a current `binary_append` ratio.

---

## 2. The ranking

Ordered by *how much of the gap it explains* × *how tractable it is*, architecture first (the
owner's stated priority). "Gap" is the Q ratio unless the column says otherwise.

| # | Bottleneck | Gap (rust/sys) | Mechanism locus | Class | Named next step? |
|---|---|---|---|---|---|
| **B1** | `gc_bound/churn` | **14.77×** | nursery sized small ⇒ capacity-forced fullsweep per op (`beam_gc` policy) | **architectural** | yes — policy lever measured (2.597 → 0.021 collections/op) |
| **B2** | copy boundary: `ets_copy` / `send_recv` / `send_recv_plain` / `etf_roundtrip` | **9.82× / 5.44× / 2.91× / 2.16×** | BIF-result copy-out + pre-copy reference walk + a three-hashing-traversal pricing walk (`beam_rt` + `beam_message` + `beam_term`) | **architectural** | yes — two levers, one already measured at ~1.2× |
| **B3** | `foreign/*` (4 real third-party modules) | **9.42× 12.65× 15.79× 19.00×** | per-instruction tax of a non-inlined opcode family + byte-at-a-time `BitWriter::push_bit` + match engine (`beam_engine`) | architectural **in shape**, small per cut | yes — 3 named levers; the big one already landed |
| **B4** | `list_walk` | **7.72×** (A/B 0.853× — improved) | baseline-tier per-element return/dispatch: driver round-trip, `jit_call_store` re-decode, `local_call_seam` per call | **architectural** | yes — 3 named levers, 1 is a contract decision |
| **B5** | `binary_append` | **≈18.1×** (single loaded round, §1.3); 40.02× in m17; A/B **0.449×** | the append itself is now linear — the residual is per-instruction cost on the baseline tier | **architectural work landed** | re-publish the ratio; the rest is B4 |
| **B6** | start-up: `baseline` p50/p99, `slice` | **2.51× / 2.56× / 2.63×** (m17) | boot-script drive + preloaded set + per-`.beam` load (`beam_rt`) | architectural **but fidelity-bound** | one lever left: the memory row |
| **B7** | compiler throughput: root descent at every heap admission | **no cross-runtime ratio** (in-crate: 3.62 % → **44.50 %** of samples) | whole-graph unscanned-root walk (`beam_rt::context::gc_walk`) | **architectural** | yes — bound/incrementalise |
| **B8** | `ets_copy` retained memory | rust peak RSS **90.30** vs sys **66.26 MiB**, linear in `N` | nursery grows and is never shrunk (`beam_rt` + `beam_gc`) | **architectural**, cross-crate | needs a new seam |

**Defer (micro-optimisation)**: family-id baking, `jit_call_store`
re-decode, `local_call_seam` boolean, block register-cache traffic (**no measurable effect**, §5), the
allocator (§5), and **Cranelift — rejected as a performance tier** (§5).

---

## 3. The entries

### B1 — `gc_bound/churn`: the nursery is too small, so every op forces a fullsweep

**Workload and both sides.** `gc_bound/churn` (allocate ~800 words, abandon them, repeat), per op:
**sys 4 257.75 ns → rust 62 478.60 ns = 14.77×** (Q; per-round 14.78 14.77 14.59).
m17 read 18.75× under load; the rust-only A/B is **0.944×** (65 009.5 → 61 461.0 ns/op, widest spread
0.93–1.00) — i.e. modestly improved, and **not** a new regression.

**Mechanism.** The collector escalates a minor collection into a fullsweep whenever the mature
generation cannot hold what the nursery is about to promote —
`crates/beam_gc/src/collector.rs:175` (`self.mature.available_words() < self.nursery.used_words()`) —
and the sizing policy that sets those capacities is `crates/beam_gc/src/policy.rs`
(`mature_after_fullsweep` `:269`, `young_after_collection` `:313`).
For churn's shape the nursery is priced so small that the escalation fires on nearly every op.

**Structural counters.** ≈**1 830 words/op** allocated, and at the runtime level **7.669165
collections/call and 1.256372 majors/call at a 609-word young space**
(`crates/beam_rt/src/runner/CONTEXT.md:431-440`; the `6.984032`/`6.996002`/`6.999200` collections and
`1.497006`/`1.499250`/`1.499850` majors columns in that section are a superseded earlier tree state).
`beam_gc`'s own attribution (`crates/beam_gc/tests/churn_attribution.rs`, 4 000 ops) gives 2.00
collections/op (1.50 minor / 0.50 major), **1 269.5 words copied/op = 1.58× the allocated volume**,
**418.0 objects/op** (≈209 per collection at 2.00 collections/op) and **634.6 words copied per
collection**; the reds door fires on **1 in 8 checks** (0.74 % of collections); `peak_young_spaces
== 1`; young capacity plateaus at 609 words.
The copier is **not** the cost: `beam_gc`'s copy loop is **4.2 %** of `perf` samples against
interpreter 20 %, libc `malloc`/`free`/`memmove` 15 %, `beam_rt::context::gc` 12.7 % and `beam_bifs`
`Vec` churn 4.9 %.
Cadence at matched policy (`+hms 233`): minors 2 591/7 785/25 967 (rust) vs 1 050/3 144/10 474 (sys)
⇒ **2.597 vs 1.047 per op**; for a live-set loop, `live/1` is **25.97 vs 1.446 per op (18×)**.

**Tried and rejected** (`crates/beam_gc/MEASURED.md`, `crates/beam_gc/CONTEXT.md`):

* *exact-live* escalation — no help for this shape and one extra young traversal per collection;
* **`need = 0`** for young occupancy in the mature size request — **every** collection escalates and
  `minor_gcs` stays **0** (cadence 1/219 vs 147/73 with `need = young`);
* mature sizing from **pre-collection** occupancy — capacity creeps 233 → 376 → 610 → 987 → … →
  6 772 (Copying) / 28 928 (Immix) on a 60-word live set (exact-live converges to 233/512);
* nursery sizing from the pre-`reset()` fill or an off-by-one `next_words` — ~20 %/collection and
  unbounded: young `heap_size` 987 → 833 026 (@N=1000) → 2 487 399 → 7 427 328,
  `VmHWM` 29.4 / 77.3 / 227.5 MiB vs the oracle's ~60 MiB;
* **write barriers** — none exist and none are possible: immutability ⇒ no mature→young pointer, so
  the collector is barrier-free by contract;
* door watermarks: `1/2` measured at **+0.048 collections/call, +8.9 % major/call** (retuned to 7/8);
  `1/1` (C-exact `used ≥ capacity`) buys nothing over 7/8 and can chain majors.

**Already landed — do not re-do it.** Post-fullsweep nursery sizing: `collect_major` used to collapse
the nursery through `young_after_collection(capacity, 0)`, discarding the settled 609-word nursery
every second op. The fix moved collections/op **2.000 → 1.500**, minors 1.500 → 1.000, kept majors at
0.500, and gave a paired wall **0.964** (15 rounds) / **0.960** (21 rounds), `task-clock` **0.929**.

**Next step.** The residual is the **nursery sizing policy**, not the two-step rule: collections scale
∝ `N`/nursery — `steady/1` 2.597 → **0.376** (`+hms 1000`) → **0.021** (`+hms 20000`), `live/1`
25.97 → 3.76 → 0.21. Raise the default young policy and/or stop shrinking the nursery between
fullsweeps. Leave the copier alone.

---

### B2 — the copy boundary: a second traversal, a reference walk, and a three-hash pricing walk

**Workloads and both sides** (Q). `copy_bound/ets_copy` **sys 222.47 ns → rust 2 180.34 ns = 9.82×**
(per-round 10.07 9.82 9.57); `send_recv` **1 503.76 → 8 154.48 = 5.44×**; `send_recv_plain`
**669.20 → 1 965.28 = 2.91×**; `etf_roundtrip` **657.87 → 1 433.16 = 2.16×**.
The difference between the two `send_recv` cases isolates the ETF component *inside* the
two-process path at **0.81 µs/op (sys) vs 6.19 µs/op (rust)**.
Code-change A/B: `ets_copy` **1.005×**, `etf_roundtrip` **1.018×** (noise — unchanged);
the two `send_recv` rows are a **small real regression**: `send_recv` 1.022×, `send_recv_plain` 1.026×,
`NEW` above `OLD` in all five rounds (sign test 5/5, ≈2 %). Reported, **not attributed** — that path is
`beam_message`/`beam_rt` work the engine crate barely touches.

**Mechanisms, each with its own attribution.**

1. **BIF-result copy-out is a second traversal** — `crates/beam_rt/src/context/calls.rs:1333`
   (`move_result_into_process_heap`), `:1700` (`copy_result_fresh`), gate at `:1863`
   (`hint_grow_gate`).
   Counter (same binary, hint off → on): **3 586.5 → 2 171.4 ns/op = 0.605×** with **refusals
   83 067 (0.332/op) → 754 (0.003/op)**, copy attempts 1.016/op on both sides, hint-driven grows
   0 → 83 387, collections/op unchanged 0.33–0.34; the sizing machinery's `perf` share goes
   **26.5 % → ~0** and wall 1 349 → 877 ms (**0.650×**). Split of `binary_to_term/1`'s 508 ns:
   prologue 82 + decode 218 + **copy-out 149** + residual 59.
2. **A reference walk before the copy-out** — `calls.rs:1115` (`graph_holds_reference`: an inline
   64-word worklist, no visited set, a `1<<20` visit budget that **declines rather than guessing**).
   A/B in one window: `etf_roundtrip` **3 164.7 → 2 144.9 ns/op (1.475×)**, `ets_copy` 7 094.0 →
   6 044.4 (1.174×), `send_recv` 15 737.9 → 14 767.4 (1.066×); negative control `send_recv_plain`
   3 236.8 → 3 319.4 (0.975×). Counter: `walk_hash_inserts` **27 → 0**.
   The residual ≈150–250 ns/op **cannot be dropped**: term identity is the address, so a container
   must be known *before* it is moved ("copy then check" is unsound).
3. **The pricing walk is three hashing traversals** — `beam_message`'s `preflight_words`
   (`crates/beam_message/src/copy.rs:347`), then `beam_term::size::size_object_bounded_with`
   (`crates/beam_term/src/size.rs:565`) → `sizing_walk` (`:386`, `HashSet` path-set + `Step::Exit` +
   `&dyn SizingPolicy`), then `is_acyclic` (`:448`, two more sets), then `size_object_with` (`:522`).
   On `ets_copy`, measured **before** the hint gate landed (`crates/beam_message/CONTEXT.md:134`),
   this machinery is **≈29 % of cycles** (`hash_one::<&usize>` 8.1 %, SipHash13 6.2 + 1.8 %,
   `sizing_walk` 2.9 %); eliminating refusals (`+hms 200000`) removes every symbol and makes the case
   **~1.2× faster**. The gate drove that machinery's `perf` share from 26.5 % to ~0, so Q's rust
   `ets_copy` (2 180.34 ns) is the post-gate build and the 29 % is not its profile.
4. **`beam_message` is not the whole story** — `copy_struct` is **198.8 ns mean / 176 ns best =
   2.8 ns/word** for a 70-word / 27-object payload (2 allocations, 192 B; five-run median,
   `crates/beam_message/PERF.md:104-108`), while a *different* counter session splits the same `send`
   as copy traversal 167.2 / whole send 190.7 / enqueue **23.5 ns** (≈88 % copy, ≈12 % build +
   enqueue, `crates/beam_message/PERF.md:111-113`) — `167.2 + 23.5 = 190.7` holds only inside that
   190.7 ns run, not against the 198.8 ns median.
   Its share of `send_recv_plain` is ≈**21 % (0.41 µs) — a lower bound** (the payload is sent twice
   per op); even a free copy leaves ≈1.5 µs, which belongs to `beam_rt`'s per-slice runner and
   `beam_process` delivery. The split quoted for a 79 518 ns/op `send_recv` (`beam_process` 73 ns =
   0.09 %, `beam_message` 439 ns = 0.55 %, remainder **99.4 %** engine/Erlang/host seams) is the
   harness's per-crate rows applied to the workload's structure, **not** a profiled end-to-end trace
   (`crates/beam_process/PERF.md:252-256`, with its own disclaimer at `:284-285`).
5. **A second copy inside the ETS registry** — `ets:lookup/2` yields `[{k, ~73-word payload}]`, copied
   onto the process's own young heap; on a full nursery `Process::grow_nursery` + one retry, with
   nothing shrinking it back (**B8**). `beam_ets` itself is **not** the cost: `TermStore::words` 73
   flat, `Table::memory` 92 flat over 1 → 1 000 000 ops.

**Tried and rejected.**

* "size first, grow, copy once" — **≈290 vs ≈121 ns/call for `lists:reverse/1` (≈2.5× worse)** *and*
  it grew the nursery every call (3 → 6 → 12 → … → 256 words);
* forcing the copy-out off entirely — `lists:reverse/1` raised `system_limit` (the pool rewind is
  load-bearing and was never removed);
* removing the caller-module float guard at the two FR-publishing GC doors — 94.68 → 95.02 MiB, no
  effect, reverted;
* an allocation-free `WorkStack` in `beam_message` (inline `[MaybeUninit<Fixup>; 8]` + spill) —
  removes every allocation yet is **payload-neutral** (187 vs 186 ns) and **17 % slower** on a 10-int
  list, so the `Vec` ships;
* "fresh payload per op ⇒ park-arm walk" — **dead**: the payload is a hoisted literal and the 101-byte
  image stays a 15-word on-heap binary; the scheduler is not the cost (`deliver`/`resume_woken`
  ≤ 0.37 %).

**Next step.** (a) a `Runtime`-owned `pid → hint` map with cleanup on process death — kills the
residual 754 refusals (≈13 ns/op, 0.4 %); (b) drop the path-set hashing from the pricing walk: a
finite `limit` already terminates a cyclic graph (≥2 words/object), and `TermGraphCyclic`/`TooLarge`
stay distinguishable through the existing error-path `is_acyclic`.
The registry's double copy is a `beam_bifs`/pool-rewind contract change — not expressible from
`beam_rt`. The ≈2 % `send_recv` regression is unattributed and worth one profiling round.

---

### B3 — `foreign/*`: the per-instruction tax of non-inlined families

**Workloads and both sides** (Q). Real third-party modules, per call:
`cowboy_bstr:to_lower/1` **337.18 ns → 3 174.91 ns = 9.42×** (per-round 10.36 9.28 9.42);
`cowboy_bstr:capitalize_token/1` **326.28 → 4 136.55 = 12.65×**;
`emqx_base62:encode/1` **210.35 → 3 343.09 = 15.79×**;
`emqx_base62:decode/1` **285.31 → 5 574.70 = 19.00×**.
Code-change A/B: **0.624× / 0.610× / 0.801× / 0.758×** — i.e. 1.6–2.3× faster than the
pre-optimization tree, so what remains is per-call host-context cost, not the foreign code.

**Mechanism.** The in-crate verdict: *"the foreign residual is not the foreign Erlang: it is the
per-instruction tax of a non-inlined family … the byte-at-a-time `BitWriter::push_bit` … and the match
engine — in that order."* The router is `crates/beam_engine/src/jit/baseline/emit/block.rs` →
`src/jit/baseline/family.rs` → `src/families/mod.rs:112` (`dispatch`: helper + re-decode + double
match); `BitWriter::push_bit` is `crates/beam_engine/src/families/bitstrings/mod.rs:778`.

**Structural counters** (in-process, release, pinned; `BENCH_TIER=baseline`).
`to_lower/1` is **438 instructions/call** (~14.6 per character) with **0** `call_bif` calls — it is a
27-clause pure-Erlang `select_val`, so no BIF is on the path; `alloc_words_rooted` **2 calls /
13 words** (the oracle's own `statistics(garbage_collection)` reclaimed count is 13.00);
`ensure_heap`/`ensure_heap_full` **0** — so the gap is **not** GC cadence for this shape;
`local_call_seam` asked **61** times/call; **3** driver round-trips and **30** in-block calls/call
(the in-block links absorb the rest).
Share by frame: `families::dispatch` **12.6 % (1 068 ns/call)** of `to_lower`'s 8.49 µs,
**11.6 % (1 275 ns)** of `capitalize_token`, 7.9 % of `encode`, 8.4 % of `decode`;
`push_bit` **6.5 % = 551 ns/call = 18.4 ns/char ≈ 2.3 ns/bit** for `<<C:8>>` (8 stores per byte);
`BitWriter::push_bits_from` **27.38 % → below the 0.8 % cut-off** and the top remaining frame is the
single `bs_init_writable` carrier allocation (`<CarrierAllocator>::alloc_impl`, **17.31 %**) — after
the append fix.
Other workloads: `encode/1` 244 instructions, 4 `+/1` BIFs on the JIT tier (20 on the interpreter,
where `+` is not inlined), 18 rooted calls / 45 words, 7 round-trips / 16 in-block calls; `decode/1`
531 instructions, 17 BIFs on the JIT tier; `capitalize_token/1` 660 instructions, 57 local-call-seam
requests.
Landed and counted: the router bypass for 20 `Bs*` variants is **−5.50 %** (`to_lower`), **−3.25 %**
(`emqx_base62`), `list_walk` 1.000000; not boxing the family helper's `Ok` is **−1.85 %** cycles;
together **−7.03 % / −4.39 %**.

**Tried and rejected** (`crates/beam_engine/src/families/bitstrings/CONTEXT.md:342-354`, explicitly
"do not re-attempt"): slice-pattern recognition of the six `bs_match` shapes → `to_lower`
**1.027× slower**; inlining `match_ctx::read_ctx`/`reread_ctx` → **1.020–1.050× slower**; testing
`small_value` before `big_parts_safe` → **1.022–1.023× slower**; and **do not inline
`bs_create_bin`** (`src/bench/CONTEXT.md:283`).
The architectural part of this row **already landed**: the per-slice off-heap mirror that made the
cost accumulate was replaced by a reservoir roster with an `O(1)` gate —
`crates/beam_rt/src/context/gc/bin_vheap.rs:203` (`OffHeapReclaimer`, `observe` `:236`, re-exported at
`crates/beam_rt/src/context/gc.rs:171`), pruned at the slice boundary by
`RuntimeRunner::reclaim_off_heap_binaries` (`crates/beam_rt/src/runner/run.rs:367`, called `:1220`) —
that is the 0.62–0.80× A/B above, and the old shape grew **23.2× over 12× `N`** (encode
16 287.5 → 377 725.2 ns/call, `crates/beam_rt/PERF.md:402`) where it is now flat.

**Next step.** (a) bake a **family id + operand-slice index at compile time** (mechanical; removes the
re-decode); (b) route a byte-aligned `width % 8 == 0` `push_uint` through the existing
`push_whole_bytes` (`mod.rs:937`) instead of per-bit `push_bit` (`mod.rs:778`), via `ensure_room` /
`source_overlaps`; (c) inline the `bs_match`/`bs_get_integer` loop — gated on B4's register-bound
discipline. Class: (a)+(b) are micro (small, safe, cumulative); (c) is architectural.

---

### B4 — `list_walk`: the baseline tier pays a round-trip and a re-decode per element

**Workload and both sides.** BIF-free list traversal, per element:
**sys 2.34 ns → rust 18.03 ns = 7.72×** (Q; per-round 7.59 7.82 7.72).
This row was **regressed** in the m17 window (11.80×, A/B 1.215×) and is now **improved**: the quiet
A/B reads **0.853×** (21.0 → 18.0 ns/element, 0.80–0.87 across five rounds), consistent with the
cross-runtime ratio moving 11.80× → 7.72×. So ~15 % of the per-element cost was given back.

**Mechanism.** On the native baseline tier a `return` costs a round-trip through the shared driver
loop (`crates/beam_engine/src/jit/baseline/mod.rs`); the call helper **re-decodes the instruction and
re-resolves the label** the emitter already resolved (`src/jit/baseline/call.rs:232` `jit_call_store`,
`:261` `jit_call_store_linked`); and every local call asks the host for the `local_call_seam` verdict
(`src/interpreter/dispatch.rs:1122`). The per-driver-loop position store + reduction charge
(`jit/baseline/mod.rs`, `set_current_code` / `ctx.reduce(1)`) is the block-entry boundary.

**Structural counters.** `driver_round_trips` **0.0001 per element** (2 per 20 000-element call) after
in-block linking, down from 23 → 2 per call (`to_lower` 33.00 → 3.00);
`local_call_seam` **1.0020 requests/element** (61 per `to_lower` call, 57 for `capitalize_token`);
`jit_call_store` **4.29 ns/element (11.85 %)** of which ≈2 ns is the re-decode;
the reds position store cost **556.1 → 610.3 instructions/element (+9.7 %)**, wall ≈1.215–1.22×, and
was **kept deliberately** — it makes a resumed pre-emption exact. The work *around* it was removed
instead, and a linked local tail-call chain is priced once (`slice.beam` `fib/1` n=18: **2 charges vs
the interpreter's 152**).

**Tried and rejected** (`crates/beam_engine/MEASURED.md`, `src/jit/CONTEXT.md`,
`src/jit/baseline/CONTEXT.md`): devirtualising the `&mut dyn ExecutionContext::call_bif` crossing
(ceiling **0.01–0.03 ns/call** — the ~2.4 ns `bif_cost` is the host's own hook body); memoising the
`LabelMap` (every sound key costs ≥ the single-pass build it replaces, and `Module` is `Clone` with
public fields); a per-`func` resolved-slice cache (**costs 20–40 %** on tight dispatch rows); a
cross-function per-run 2-slot memo (answered 58/60 resolutions yet **+1.84 %**; kept test-only); a
range-crossing Y-window witness (**+1.15 %** and **unsound** at the `fold_step` `pc+1` shape);
removing the Y-window derivation while keeping the position store (invalid split — with no window
published the emitted Y guard rejects, the tier deopts, `result_is_correct = false`; the store itself
is not a removal candidate, `src/jit/CONTEXT.md:258-262`); Cranelift as a tier (§5).

**Next step.** Give `JitRuntime` the active function's `entries` base and answer the **block entry
address** for a same-function continuation — the instruction-count tree already prices it
(556.1 → 610.5 → 570.9 → 530.6 → **497.3** instructions/element); then pass `target_func`/arity as
block-ABI immediates so `jit_call_store` stops re-decoding.
The `local_call_seam` verdict is a **contract decision** (it is intentionally non-idempotent): a
per-`Module` bool would collapse 1.0020 requests/element, but the tier must not short-circuit it
without the parent's agreement.

---

### B5 — `binary_append`: the append is linear now; the residual is per-instruction cost

**Workload and both sides.** `binary_append` (a 16 384-segment writable-accumulator squeeze, 65 536 B
out per call). Last full-record cross-runtime figure: **sys 108 253.0 ns → rust 4 220 897.5 ns =
40.02×** (m17, load 3–92, **before** the writable-append work landed).
Fresh single round on this host (§1.3, load 161–181): **sys 140 649.11 ns → rust 2 547 595.26 ns
≈ 18.1×**.
Code-change A/B (quiet): **2 964 037 → 1 333 404 ns/call = 0.449×**.

**Mechanism — and why this entry changed.** The writable accumulator **does** grow in place:
`raw::InPlace::grow` → `Machine::grow_writable_payload` → `beam_alloc::RefcBinary::ensure_capacity`
(`crates/beam_engine/src/families/bitstrings/create/mod.rs:68-69`, `:95-107`), a
`private_append` in the **first** segment position over a payload this machine allocated is written in
place (C's `erts_bs_private_append_checked` writable arm, `erl_bits.c:1943-2020`), and the growth door
is consulted **only on a shortfall**. `bs_init_writable` seeds the payload at the size operand or
`DEFAULT_WRITABLE_SIZE = 1024` bytes (`create/mod.rs:162`, `:199`).
What is left is the per-instruction cost of the enclosing loop on the baseline tier.

**Structural counter that proves it.** On `long_append:expand/1`: 98 318 instructions/call
(6/element), 16 384 appends, final length 65 536 B, **2 in-place grows + (4) moves, 61 440 B copied in
total** (`create/grow_tests.rs`) — against a copy-rebuild counterfactual of **536 903 680 B**, i.e.
~8 300× more bytes. The driver's own note for the §1.3 run states the measured path is the binary
append, not a series of copies.
Hard constraint: `n_rust` is capped at **2 000** because a larger `N` exceeds `beam_rt`'s
non-overridable `RUN_DEADLINE = 60 s` (`crates/beam_rt/src/runtime/scheduler.rs:90`), so any increase
in per-call cost silently turns this row into `failed(no-binary-append-run)`.

**Tried and rejected.** Nothing on the growth path was rejected — it was implemented and measured
(0.449×). Rejected: raising `n_rust` past 2 000 (deadline, non-overridable); quoting
`ns_per_call` across runs at different `N` (rust's figure is `N`-dependent).

**Next step.** (a) **re-publish the ratio** with a matched round (Q's row set did not include this
workload, so the current rust/sys figure is not on record); (b) the remaining per-call cost is
**≈19.95 ns/instruction** on the baseline tier, i.e. the same lever as B4 (block-entry address +
call-store immediates) — do not look for another append-specific fix.

---

### B6 — start-up is 2.5× the oracle, and that is mostly fidelity

**Workloads and both sides** (m17 — Q does not cover start-up).
`baseline` start-up **p50 127.3 ms → 323.6 ms = 2.51×**, **p99 137.5 → 335.2 = 2.56×**;
`slice` (one call per process, nine cases) **134.1–137.9 ms → 316.2–368.8 ms = 2.45–2.68×
(median 2.63×)**; the like-for-like `slice` peak RSS is **68.0 → 120.0 MB = 1.76×** (§5 says why the
loop workloads' RSS is not a memory comparison). Compute-loop p50 193.5 → 348.3 ms (1.84×) is
native-floor dominated, not interpreter throughput.

**Mechanism.** Every action except `-version` drives the real boot script — 22 directives, for both
runtimes (`crates/beam_rt/src/boot/drive.rs`, `bootscript.rs`) — and the whole process is **≈0.34 s
wall / 0.33 s CPU**, CPU-bound, **0 `clock_nanosleep`**, 927 filesystem stats (2 on `os.beam`), **98
`.beam` opens**. The oracle boots too, so most of this cost is *supposed* to be there; the gap is
per-directive and per-load overhead.

**Structural counter / landed.** The port-poll gate was the big measured win: the boot's *idle*
`inet_gethost` port used to cost a 20 000-round ping-pong **1 063 blocking waits / 26.9 s**; made
progress-aware it is **6 waits ≈ 137 ms**, `strace` shows **0 `clock_nanosleep`** over 200 000 round
trips, and boot `epoll_wait` total fell **130.8 → 4.1 ms** over the same 15 calls.
Same-binary pair (poll gate off → on): p50 **421.7 → 298.6 ms (0.708×)** and `slice` summed
3 880.2 → 2 625.2 ms ⇒ the oracle multiple moved **3.741× → 2.649×** (slice 3.088× → 2.089×).
m17's A/B also puts HEAD at **1.047×** the m15-era build's p50 (m16 said 1.17×).

**Tried and rejected.** Eager whole-module JIT compilation at first call — the measured cause of
**11–12× one-shot regressions** (`string_str_length` 11.27×/12.34×); replaced by lazy per-function
compilation, now ≈**1.2–1.25×** over the interpreter (31.86 ms default vs 25.51 ms interpreter).
A static per-module switch-safety predicate that pinned modules to the interpreter for life —
rejected. The m9 hypothesis that lazy compilation caused a `slice` rise — **a load confound, not
real**. The interpreter tier as a performance option — measured worse on every case.

**Next step.** No large lever is left that is not fidelity work: the residual is the preloaded set
plus per-`.beam` load/relocation, and the only genuinely attackable row is the like-for-like memory
figure (`slice` 1.76×) → **B8**. Re-run the tier control before quoting any tier figure: m17 ran every
rust pass at the default tier and has **no** `BEAM_RT_TIER=interpreter` comparison.

---

### B7 — compiler throughput: a whole-graph root descent at every heap admission

**Workload and both sides.** **No cross-runtime ratio exists** — this is an in-crate profile exposed by
the in-process-compilation work. After the nursery fix, `beam_rt::context::gc_walk`'s
`Visited::insert` went **3.62 % → 44.50 %** of samples, `Walk::run` 2.40 → 10.85 %,
`beam_term::layout::for_each_child_slot` 1.23 → 8.22 %, `gc::root_is_unscanned` 0.75 → 6.00 %.

**Mechanism.** `crates/beam_rt/src/context/gc_walk.rs` (`Visited` `:629`, `insert` `:692`) walks the
whole graph looking for roots that have not been scanned yet, at **every heap admission**; the cost is
superlinear — **×4.67 cost for ×1.75 lines** of input.

**Why it is worth fixing.** It is the current ceiling on compiler throughput, and the fix that
exposed it was worth more than this regression costs: nursery `contains` became indexed
(`BTreeMap`, `crates/beam_gc/src/nursery.rs:221`, self cost **60.94 % → 1.44 %**) and
`compile:file/2` wall times fell (`ordsets` **254.25 s → 13.99 s**, `timer` **>600 s DNF → 65.32 s**)
with byte-identical output.

**Tried and rejected.** Nothing on the walk itself — it is newly exposed. Reverting to the unindexed
nursery `contains` is rejected by the numbers above.

**Next step.** Bound the descent (cap it, or make it incremental across admissions), or replace the
per-admission whole-graph scan with a root-set publication at the points that already exist. Measure
with the compiler route — no `bench` workload isolates it.

---

### B8 — `ets_copy` retains memory: the nursery grows and is never shrunk

**Workload and both sides.** `copy_bound/ets_copy` peak RSS **rust 90.30 MiB vs sys 66.26 MiB**, both
read at **`N` = 100 000** (`crates/beam_ets/PERF.md:39`) — one `N`, so it is a ratio, not a curve. The
growth is **linear in `N`** at ≈**790 B ≈ 99 words retained per iteration**
(`crates/beam_rt/PERF_DISPATCH.md:305-306`), and a second session read **94.68 MiB at that same
`N` = 100 000** (`crates/beam_rt/PERF_DISPATCH.md:305`) — a session delta, not `N`-growth. The
linearity's real anchor is the per-iteration retention plus the oracle, whose peak RSS moves
**+0 KiB** over a 20× `N` range (counter below).

**Mechanism.** `move_result_into_process_heap` (`crates/beam_rt/src/context/calls.rs:1333`) **cannot
collect at a BIF call** — the machine's roots are not published there — so a full nursery is met by
`Process::grow_nursery`, and `beam_gc::Nursery` **retains** the retired space. On this route the
native tier never reaches `ensure_heap_full`'s collect (`crates/beam_rt/src/context/gc.rs:1268`) nor a
slice-end reds door, and `+hmax` does not bound the growth-retired nursery chain.

**Structural counter.** The "adopt path / rewind guard" hypothesis is **refuted** (`safe_to_rewind` is
already true, the pool is already rewound). On the ETS side everything is flat: `TermStore::words` 73
and table memory 92 over 1 → 1 000 000 ops, `VmRSS` growth 1 376 KiB @100 000 / 1 412 KiB
@1 000 000, while the oracle's peak RSS moves **+0 KiB** over a 20× `N` range. So the growth is on the
caller side.

**Tried and rejected.** The ETS-interior hypothesis (interiors are right and flat, §5).

**Next step.** Either a `beam_process` seam to shrink/reuse a retired young space, or a
`beam_engine`/runner seam that publishes the machine's roots at the BIF call so the collection can
happen there. Cross-crate, architectural.

---

## 4. Identified but not yet measured

Do **not** present these as bottlenecks with numbers — they are per-slice constants, so they matter
for `slice`/start-up-heavy routes and would be invisible on the loop workloads.
Measure with `slice`, not `list_walk`.

* a `String` allocation per slice in `RuntimeContext::enter_module`
  (`crates/beam_rt/src/context/runtime_context.rs:532`);
* a fresh `RuntimeContext` + `DispatchMemo` per slice;
* `publish_parked_roots` copying the whole x register file (up to 1 024 words) + stack;
* engines inserted/removed in a `HashMap` per slice.

---

## 5. Explored and rejected — do not re-propose

Consolidated so a future round does not spend a day re-measuring a dead end.

**Execution engine / JIT**

* **Cranelift as a performance tier — rejected.** Cost ratio Cranelift ÷ baseline: `walk/2` **0.48×**
  (its only win), `slice:fib` 0.95, `aggr` 0.99, then **1.07–2.50×** (`sum/1` 2.50); the
  position-balanced ABBA harness (15 rounds × 2 samples/tier, bracketing three runs) gives `aggr`
  0.988–1.009× (tie), `arith/2` **1.616–1.621×**, `cmp` **1.364–1.422×**. One-time compile is
  **6.5–60×** the baseline's. `jit-cranelift` stays default-OFF and is **not reachable end-to-end**:
  `beam_rt/src/runner/tier.rs` maps `BEAM_RT_TIER` only to `JitMode::{Auto,Interpreter,Baseline}`.
* The five dispatch/memoisation ideas and the Y-window witness in B4.
* **Block register-cache traffic — below this host's resolution.** Spill stores **4 678 655
  (written-set) → 1 594 678 (dirty-set)** (−65.9 %) and bytes 341 653 629 → 320 065 790 (−6.32 %);
  prologue/push+pop ops 7 167 996 → 2 707 930 (−62.2 %) — yet 9 paired rounds across loadavg 8 → 123
  flip sign per case with a 1.5–5.2× spread ⇒ **≲2 % wall-clock, no attributable movement**
  (`crates/beam_engine/MEASURED.md:142`). Also rejected: per-helper clobber
  sets narrower than the whole cache (no sound set — a collecting host may rewrite every root),
  changing the 5-slot `CACHE_REGISTERS` budget (no per-register benefit counter), and Y-slot caching
  (12 Y accesses corpus-wide; attempted then reverted).
* `CodeCache` per-slice IR re-hash — **closed**: was ~2 µs tiny / ~141 µs at 2 048 instructions, now
  **0** via a load-time MD5 digest key (probe: interpreter 351.19 vs baseline 101.70 ns/slice-entry).
  `remember_module` priming has no production caller; the address memo is redundant on the digest path.
* The `src/bench` harness's own `format!` key — a **measurement trap**, not engine cost: a per-dispatch
  `format!` was 14.2 % of `list_walk` samples; fixed, `list_walk` 20.90 → **14.65 ns/instruction
  (1.43×)**, `encode` 1.24×. Never quote a pre-fix figure as engine cost.

**GC**

* **MMTk and Boehm — rejected outright**; GenImmix stays a measurement-gated option.
* Immix below one whole block is **strictly dominated** (64 → 64 / 12 → 64, 128 → 128 / 24 → 128).
  From one block up it pins the live set: the allocation-heavy A/B moved **19 115 520 (Copying) vs
  12 290 560 (Immix) words = 0.643×** at **3.79× peak mature occupancy** (7 168 words vs 27 136
  line-words / 1 696 lines) and **2.35× peak resident words, nursery + mature** (14 848 vs 34 816)
  (`crates/beam_gc/MEASURED.md:81` and `:82`);
  runtime-shaped through `beam_rt`'s doors, `initial_mature_words` 2047 vs 2048 is **1.146× words
  moved for 6.57× occupancy**. `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS = 2048`
  (`crates/beam_gc/src/selection.rs:111`; `fn select_mature` `:122`) stays the rule.
* The B1 list: `need = 0`, exact-live escalation, pre-collection mature sizing, pre-`reset()` nursery
  sizing, the `1/2` and `1/1` door watermarks, and write barriers (impossible by contract).

**Messaging / terms / ETS**

* The allocation-free `WorkStack` and the "fresh payload per op" hypothesis (B2).
* Reading `vm_memory_total_bytes` as a footprint win — **an accounting artefact**, see below.
* `beam_ets` interiors: the B+-tree is the right `ordered_set` — like-for-like at `n` = 100 000
  (shuffled `smallint`) insert/delete is **176.8/217.0 ns (tree) vs 5 221.1/5 146.0 ns (sorted
  `Vec`)**, i.e. **29.54/23.72×** (`crates/beam_ets/PERF.md:267` and `:268`), and it stays at or below
  parity at 1k/10k (0.92/0.83 @1k, 3.51/3.18 @10k), worst 115.92× (100k descending). Only the tree
  was measured at 1M — insert/delete **297.4/460.0** (`PERF.md:306`) — because **no** sorted-`Vec` 1M
  figure exists: extrapolating its measured `O(n)` 5.2 µs at 100k gives ≈52 s per metric per pass
  against the tree's measured 0.3 s (`PERF.md:300-302`). Copy-trimming/hysteresis is in place
  (200-round churn: `max == base`, no thrash).
* Map flatmap→HAMT threshold (`MAP_SMALL_MAP_LIMIT = 32`, `crates/beam_term/src/map.rs:111`) — no
  measured threshold bottleneck; the measured map cost is the 64-key HAMT copy (234 words / 3 744 B /
  559.3 ns) inside B2.

**Allocator**

* `beam_alloc`'s per-op figures (size-class lookup 3.58 / 5.55 ns; alloc+free 871.14 ns at 1×64 B,
  616.16 ns at 256×64 B) are **memory/syscall-bound, not CPU-bound** — documented once as "do not
  chase these as regressions" (`crates/beam_alloc/CONTEXT.md:262`), with the numbers themselves
  designated a regression *baseline* in the harness's own method notes
  (`crates/beam_alloc/tests/hot_path.rs:55-58`). Release ≈ debug, and the jump is the legacy-faithful
  `smbcs` floor — the implemented rule is `mbc_size`'s clamp of the preferred carrier into
  `smbcs`..`lmbcs` (`crates/beam_alloc/src/allocator/core.rs:392-416`; `get_next_mbc_size` is the
  legacy C name, not an item in this crate) — plus fresh-carrier page faults. The record carries the
  jump pair in two spellings: `1 062 / 4 290` (`CONTEXT.md:258`) and `1 029 → 4 311`
  (`CONTEXT.md:260`), so do not quote either as a settled pair.

**Bench instrumentation**

* The pre-m15 **sequential all-`N`-then-all-`0` mean schedule** — it published `etf_roundtrip` at
  **136.3×** where the interleaved estimator gives ≈2.4×. `marginal.rs` interleaves within a round,
  takes paired differences with a median+MAD gate, and withholds below `3 × max(1 ms, 1.4826·MAD)`.
  **Do not revert it.**
* Runtime-major `--runtime all` passes as a headline source (up to 18 % ordering bias, m16).
* Raising `binary_append`'s `n_rust` past 2 000 (the 60 s `RUN_DEADLINE` is non-overridable).
* Quoting `foreign`/`binary_append` `ns_per_call` across runs at different `N`, and cross-session
  absolute wall-time diffs.

---

## 6. Honest negatives and traps

* **`erlang:memory(total)` is 5.41× *lower* on rust (6.69 MB vs 36.20 MB) — an accounting change, not
  a memory win.** The A/B puts it at a flat **0.506×** (13.22 → 6.69 MB) while OS peak RSS **rose
  1.13×** (133.2 → 150.4 MB). The comparable footprint rows are `slice` (1.76×, rust worse) and the
  A/B's `copy_bound` 0.35× / `foreign` 0.61×.
* **Loop-workload peak RSS is not a memory comparison.** `list_walk`/`gc_bound` 1.75× are
  boot-dominated; `copy_bound` 2.34×, `foreign` 2.91×, `binary_append` 3.73× additionally pay per-op
  retention. `slice` (`N`=1) is the comparable row.
* **A "neutral" change is not neutral until the A/B says so** — the two `send_recv` rows are a real
  ≈2 % regression (5/5 rounds), and `list_walk` was the regression of the m17 window until the
  current A/B (0.853×) closed most of it. Check those rows first.
* **The external targets are host-starved figures in m17, not regressions.** `erlc_compiler`
  7.045 files/s, `elixir_compiler` 2.126 files/s, `cowboy` 38 297 req/s were measured at load 64–73,
  with the report's own control (`elixirc` compiler start-up 828.8 ms vs m16's 288.1 ms = 2.9×).
  m16's low-load values remain the reference (`erlc` 9.618 files/s, `elixir` 4.897 files/s,
  `cowboy` 194 330.88 req/s); `khepri` 78 526.37 ms and `plb` 68 293.09 ms are m16's; `emqx` is an
  honest `skipped(emqx-requires-otp-28)`.
* **The bench row set is not the whole product.** Q measures seven workloads; compiler throughput
  (B7) and the external targets have no cross-runtime ratio at all, and `binary_append` has no Q row
  (B5).

---

## 7. Reproducing a figure

```sh
# build the driver and the runtime
erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench \
    --manifest-path erts_rust/Cargo.toml --offline

# one workload, both sides, one process at a time, pinned
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust \
    --workloads gc_bound --out-file $TMPDIR/gc-rust.json --verbose
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime sys \
    --workloads gc_bound --out-file $TMPDIR/gc-sys.json --verbose

# compare reports
erts_rust/target/release/bench compare $TMPDIR/gc-sys.json $TMPDIR/gc-rust.json

# in-crate harnesses (no beam_rt boot in the timed loop)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_bstr_to_lower_decomposition

# the execution tier (rust-only; re-run before quoting any tier figure)
BEAM_RT_TIER=interpreter taskset -c 3 setsid -w erts_rust/target/release/bench run --quick \
    --runtime rust --workloads $WL --out-file $TMPDIR/tier-interp.json
```

Record `/proc/loadavg` before and after every pass; `bench` honours `BEAM_RT`, `ERTS_ORACLE`,
`ERLC`, `BEAM_RT_TIER`, `BENCH_*` and `--repo <id>=<path>`; it never sets `ERTS_STDLIB_DIR`.
Raw artifacts belong under `tools/bench/results/` with only mechanical normalisations —
**never edit a metric value**.
