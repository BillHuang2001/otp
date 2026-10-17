# `beam_message` — Measured: per-send copy cost (single-pass copy, the fix-up worklist, the shared-term test)

The measurement artifact for the `beam_message` PERF change that relaxed the copy engine's
single-pass fast-path gate from a **clean destination** (`Heap::used() == 0` **and** an empty
off-heap table) to **any destination whose off-heap table is empty** — a dirty destination
**heap** included.

The crate `CONTEXT.md` carries only the load-bearing conclusions; every number lives here and is
**measured, never modelled**.

## 1. The two sides being compared

| side | path | when it fires |
|---|---|---|
| **gate-locked two-traversal path** (BEFORE) | `preflight_words` sizing walk **then** the `shallow` + worklist write walk | any destination with `used() > 0`, even with an empty staging off-heap table |
| **new single-pass path** (AFTER) | one `try_copy_fast` traversal through `TrialTarget` + one `alloc_words` commit | any destination whose off-heap table is empty — clean **or** dirty heap |

Why the OLD gate was wrong for a real send: `beam_process::Process::copy_message`
(`erts_rust/crates/beam_process/src/process.rs`, ~line 1528) copies into
`collector.young_mut().heap_mut()` — the receiver process's **long-lived** young heap, which is
dirty (`used() > 0`) after the very first message — while its staging table is a fresh
`let mut dst = OffHeapTable::new();`. So `send_recv` effectively never reached the fast path and
paid a second full traversal on every send.

Why the NEW gate is sound (and why the old failure-branch `to.reset()` had to go):
`TrialTarget` hands out only `[to.hp(), to.hp() + to.available())`, i.e. the heap's **unused tail**;
it counts `written` without touching `used`/`peak`. On success the single commit
`to.alloc_words(written)` hands out exactly those words from that same `hp`, so the committed region
holds the copied graph. On failure nothing was committed and the leftover words lie beyond `used` —
invisible and overwritten by the next allocation — so there is **no heap rewind to perform**, and
`to.reset()` would be *wrong* once the heap can be dirty (it would discard the live `used` words).
Only `OffHeapTable::rollback()` runs, and that is sound precisely because the gate guarantees the
table started empty.

## 2. Host and method

**Host:** shared/oversubscribed sandbox (no CPU pinning), dev profile (`opt-level=1` for the crate,
deps at `opt-level=2`). `/proc/loadavg` is printed by the test itself at start and end; the range
over this set of runs was ≈ **5.9 – 10.5** (1-minute) — a single sample is meaningless, so each
figure is the **mean over an iteration batch, taken 3 times**, and read together with `best_ns`
(the minimum single-iteration time, the least load-sensitive signal).

**Method** (`tests/send_perf.rs`): each shape is built once on a source heap and then sent
`ITERATIONS` times through the **real** `beam_message::send` path — the copy, the destination
off-heap drain and the `Message` build (`enqueue_copied`). Both destinations are given **identical
free head-room** (`size_of_copy(term) + HEADROOM`, `HEADROOM = 64` words) and a fresh empty staging
`OffHeapTable`:

* **clean** — brand-new heap, `used() == 0`;
* **dirty** — the same capacity, with **one word pre-charged** so `used() > 0`.

The clean-vs-dirty delta therefore isolates exactly the preflight traversal the change removes.
The test asserts **correctness only** (never a timing threshold): the enqueued payload is
structurally `compare::eq` the original, `queue.len() == 1`, and `dst.used()` advanced by exactly
`size_of_copy(term)`.

Shapes (`words` = `size_of_copy`, `objects` = copied object count):

| shape | words | objects | what it exercises |
|---|---|---|---|
| small tuple | 4 | 1 | the fixed per-copy constant |
| 10-int list | 20 | 10 | a cons chain |
| 8 × 32-byte binaries | 72 | 16 | on-heap + off-heap (refc) binaries |
| HAMT map (64 keys) | 234 | 65 | a >32-key HAMT trie |
| send_recv payload | 70 | 26 | the exact `tools/bench` `send_recv` payload: a 5-key flat map `#{a => [10 ints], b => {ok, <<"hello world">>, 3.5}, c => <<12 bytes>>, d => "a char list", e => -42}` |

## 3. Before → after (mean ns/op, 3 runs each; `best_ns` from a further run)

BEFORE — gate = `used() == 0 && empty off-heap table`; `/proc/loadavg` ≈ 9.0 / 7.75 / 5.87:

| shape | words | clean (mean, 3 runs) | dirty (mean, 3 runs) | dirty − clean |
|---|---|---|---|---|
| small tuple | 4 | 79.6 / 76.5 / 78.3 | 91.9 / 84.2 / 106.4 | +12.3 / +7.7 / +28.1 (10–36 %) |
| 10-int list | 20 | 129.9 / 124.4 / 220.5¹ | 233.2 / 212.3 / 282.6 | +103.3 / +87.9 / +62.1 (28–80 %) |
| 8 × 32-byte binaries | 72 | 311.2 / 309.2 / 339.3 | 616.9 / 594.9 / 622.2 | +305.8 / +285.7 / +283.0 (83–98 %) |
| HAMT map (64 keys) | 234 | 1085.6 / 1050.7 / 1071.2 | 2169.4 / 2159.5 / 2231.4 | +1083.8 / +1108.8 / +1160.2 (≈ 100 %) |
| send_recv payload | 70 | 366.3 / 360.9 / 385.5 | 740.3 / 693.1 / 705.8 | +374.0 / +332.2 / +320.3 (83–102 %) |

¹ the `220.5` is the one load-spiked entry; the other two runs agree (≈ 125).

AFTER — gate = empty off-heap table only; `/proc/loadavg` ≈ 9.49 / 7.92 / 5.96:

| shape | words | clean (mean, 3 runs) | dirty (mean, 3 runs) | dirty − clean |
|---|---|---|---|---|
| small tuple | 4 | 82.0 / 87.4 / 141.6 | 71.3 / 94.8 / 92.8 | noise |
| 10-int list | 20 | 110.9 / 117.1 / 132.3 | 111.6 / 120.5 / 121.3 | +0.7 / +3.4 / −11.0 |
| 8 × 32-byte binaries | 72 | 282.9 / 335.7 / 323.3 | 281.8 / 326.9 / 320.5 | ≈ 0 |
| HAMT map (64 keys) | 234 | 990.9 / 1093.4 / 1092.7 | 1002.2 / 1098.0 / 1116.5 | ≈ 0–2 % |
| send_recv payload | 70 | 351.6 / 376.9 / 385.8 | 363.5 / 381.6 / 384.5 | ≈ 0–3 % |

A further AFTER run at `/proc/loadavg` 10.49 (means noisy, so `best_ns` shown) confirms the
collapse of the delta — clean and dirty are now the same path and cost within noise:

| shape | words | clean `best_ns` | dirty `best_ns` |
|---|---|---|---|
| small tuple | 4 | 87 | 88 |
| 10-int list | 20 | 159 | 156 |
| 8 × 32-byte binaries | 72 | 322 | 314 |
| HAMT map (64 keys) | 234 | 1144 | 1155 |
| send_recv payload | 70 | 407 | 401 |

**Reading:** BEFORE, a dirty destination cost **1.1×–2.0×** a clean one (the entire second traversal
was avoidable — the staging table was always empty). AFTER, the dirty row matches the old *clean*
fast-path figure for every shape, and the clean-vs-dirty delta is noise. Nothing regressed: the
clean path keeps its single traversal, and every typed error contract is unchanged (a refused copy
still writes nothing — pinned by `tests/single_pass.rs` and `tests/sizing.rs`).

### Isolated `copy_struct` preflight share (`tests/perf.rs`)

`copy_struct` alone, single-pass vs sized (a **non-empty** off-heap table forces the two-traversal
path), `best-of-100 000`, `/proc/loadavg` 10.49 / 8.27 / 6.18:

| shape | single-pass `ns/iter` | sized `ns/iter` | size-walk share |
|---|---|---|---|
| small tuple (4 words) | 41.5 | 55.9 | 14.4 ns (35 %) |
| flat map, 8 keys (20 words) | 80.1 | 112.0 | 31.9 ns (40 %) |

This is the traversal cost that the old gate charged the *dirty* `send_recv` destination and the new
gate does not.

## 4. Reproduce

```sh
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message --test send_perf -- --nocapture
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message --test perf -- --nocapture
```

Always go through `erts_rust/scripts/cargo.sh` (`$HOME` is read-only); never `cd`. The crate's tests
are **all self-checks** (structural `compare::eq`, word-count arithmetic, refcounts) — no test in
`src/` or `tests/` invokes `erl`/`erlc`. Absolute numbers are inflated under the shared host —
prefer ratios.

## 5. What remains (cross-crate — reported, **not** fixed here)

The `copy_bound` `send_recv` workload is **5.26×** the system-ERTS oracle and `send_recv_plain`
**2.92×** (`erts_rust/tools/bench/RESULTS.md` m15: 7 696.8 vs 1 464.3 ns/op, and `../beam_rt/src/runner/CONTEXT.md`
*Measured*: ≈ 1 943 ns/op rust vs 666.5 ns sys). This crate's share of `send_recv_plain` is the whole `send`
for that payload — **≈ 210 ns in release** (§7), i.e. **≈ 11 %** of the per-op figure, so even a free copy
leaves the workload at ≈ 1.73 µs/op. The residual is the sender/receiver schedule, owned by:

* **`beam_rt` — the per-slice runner/pass work.** `../beam_rt/CONTEXT.md` and its
  `src/runner/CONTEXT.md` *Measured* enumerate it: the engine moved out of/into a `HashMap` per slice, a
  fresh `RuntimeContext` + `DispatchMemo` per slice, `ctx.enter_module`'s `String`, the park/resume root
  publish, the receive-timeout timer heap, and the per-boundary wake sweep. That is where the two-process
  gap lived: an allocation-door "decline circle" (29 923 arena fallbacks per 30 000 iterations), park
  hand-offs that copied the 1024-word x file twice per iteration, unbounded register-file scans and a timer
  tombstone leak — fixed there, taking `send_recv_plain` from 10 127 to ≈ 1 990 ns/op (**−80 %**).
  No part of that per-op cost is a pass sleep: the run route's pass wait is progress-aware and issues
  **0** `clock_nanosleep` on a `send_recv`-shaped run, and `PORT_POLL_SLICE` is a non-destructive
  zero-timeout probe (`../beam_rt/CONTEXT.md`).
* **`beam_process` — the delivery path.** `Process::copy_message` (the dirty young-heap destination above),
  `Process::deliver`/`deliver_message`/`run_rooted_collection`, and the mailbox + `QueueCursor`
  selective-receive scan.
* **`beam_codec` — the ETF codec**, for the `{SR}` case only: `tools/bench/src/workloads/copy_bound.rs`'s
  `send_recv` round-trips the payload through `term_to_binary`/`binary_to_term`, which the `send_recv_plain`
  case does not (that is what the 5.26× vs 2.92× split measures).

**Measurement this crate can offer those nodes:** `tests/send_perf.rs` is that micro-benchmark already — a
real `send` into a `words + 64`-word destination over five shapes, `taskset -c 3`-pinnable. Attribution of
the remaining per-op cost must come from counters in `beam_rt`'s runner, not from this file.
## 6. Measured and reverted: the allocation-free fix-up work-stack

The copy path's worklist is the per-call `Vec<Fixup>` in `src/copy.rs`. An allocation-free replacement
— a `WorkStack` (`[MaybeUninit<Fixup>; WORK_INLINE = 32]` inline + a `Vec` spill, one `push`/`pop`
compare on the inline half) — was implemented, measured, and **reverted**: it removed the worklist's
heap allocations (counted deterministically: the copy path went from 2 allocation events per
`copy_struct` on every standard shape — 4 on the 64-key HAMT — to **0**), but it measured *slower* on
the crate's own list-shaped copies, and no cheap variant of it removed that.

**Controlled A/B.** Two pre-built test binaries per side, one process at a time, `taskset -c 3` pinned
(this host is heterogeneous: cpu0–3 are 4.7 GHz P-cores, cpu4–15 are 3.3–3.7 GHz E-cores, so
unpinned runs land on different core types — that is what made the first readings look noisy).
Sides: `base` = the `Vec<Fixup>` worklist (predecessor of the work-stack commit); `#[cold]` = the
merged `WorkStack` with `#[cold]` spill arms; `ship` = the same with the `#[cold]` attributes dropped;
`w1` / `w3` = `WORK_INLINE = 8` / an initialised inline array. Dev profile (`opt-level = 1`, the
profile `tests/perf.rs` runs under), 5 rotated rounds per side, `--test-threads=1`,
`/proc/loadavg` 11.96–13.14 throughout, `best_ns` (mean of the 5 rounds; the per-round values do not
overlap between `base` and any `WorkStack` side):

| shape (test) | `base` | `ship` | `#[cold]` | `w1` (cap 8) | `w3` (init arr) |
|---|---|---|---|---|---|
| deep list, 2000 cons cells (`perf`) | **10 919** | 13 676 | 23 226 | 13 596 | 13 540 |
| HAMT map, 64 keys (`perf`) | **877** | 977 | — | 1 039 | 970 |
| flat map, 8 keys (`perf`) | **83.2** | 86.8 | — | 87.2 | 96.2 |
| small tuple, 4 words (`perf`) | **25.4** | 27.8 | — | 25.8 | 41.2 |
| 10-int list (`send_perf`) | **98.6** | 104.4 | — | 103.8 | 119.2 |
| HAMT map, 64 keys (`send_perf`) | **890** | 949 | — | 988 | 957 |
| send_recv payload (`send_perf`) | **304** | 312 | — | 310 | 318 |
| 8 × 32-byte binaries (`send_perf`) | 240 | **208** | — | 210 | 213 |
| 64-key HAMT, first allocating arity | 32/33 boundary | 32/33 | 32/33 | 8/9 | 32/33 |

The deep-list regression is **real and reproducible in this profile** (`base` 10 619–11 371 over the 5
rounds vs `ship` 13 327–14 299, plus a separate 15-round unpinned `base`-vs-`#[cold]` run at 10.4–17.0 µs
vs 20.3–24.6 µs): ≈ **+25 %** on the deep list, ≈ +11 % on the 64-key HAMT, ≈ +1–6 % on the rest, with
only the `8 × 32-byte binaries` shape faster (−13 %). It is a **dev-codegen** effect, not algorithmic:
the shape takes the **single-pass `try_copy_fast`** path (empty off-heap table — `Path::SinglePass` in
`tests/perf.rs`), so no preflight/sizing walk is involved; with `#[cold]` the inlined `pop` round-trips
the popped fix-up through a stack slot, and dropping it restores the register form but leaves the
residual. `w1` (a 128 B frame instead of 512 B) and `w3` do not help either, and `w3` additionally
regresses the small-tuple and flat-map shapes — so the residual is not frame size and not the
`MaybeUninit` initialisation.

At `--release` (`opt-level = 3`, `lto = "thin"`) the picture inverts: the deep list is a wash against
`base`, and the real message payloads are faster with the work-stack (`HAMT` and `send_recv` payload
−14 % to −22 %, `8 × 32-byte binaries` −15 %; those runs were taken at a higher `loadavg`, 34–58, so
treat the magnitudes as indicative). A future pass that wants the allocation-free worklist must
therefore justify it against **that** profile (where the crate ships) with release-profile evidence,
not against the dev-profile figures this file records — and must keep the `Vec<Fixup>` baseline in the
same A/B. Until then the `Vec<Fixup>` worklist ships: it is non-regressive in every measured shape and
profile except the release payloads above, and this crate's benchmarked numbers are the dev ones.

**A later release-profile A/B of the same idea** (`WORK_INLINE = 8`, `[MaybeUninit<Fixup>; 8]`, 0
allocation events per `copy_struct` on every shape but the 64-key HAMT, deterministic counters
confirming it) does **not** reproduce the inversion: against the `Vec<Fixup>` baseline at
`/proc/loadavg` 17–23, `taskset -c 3`, 6 interleaved pairs, the `copy_bound` payload was *neutral*
(best-of-20 000 187 vs 186 ns) and the `10-int list` was **17 % slower** (best 90 vs 76). An inline
buffer of either size therefore does not ship; the allocation is not where the per-send cost is
(§7 — removing a single tag test per object beats removing both allocations).

## 7. Measured and shipped: no shared-term re-test on the worklist pop path

A fix-up is only ever recorded for a child `push_fixup` has already proven **not** shared, so the copy
loop's pop side was re-deriving what `shallow` recomputes at its head. `shallow` keeps that guard for
the two root calls; the loops call `shallow_unshared` — one `is_shared` (four tag tests) less per copied
object, no added state, no allocation change.

**Controlled A/B.** Two pre-built `send_perf` binaries (`before` = the `Vec<Fixup>` loop calling
`shallow`, `after` = the split), one process at a time, `taskset -c 3`, alternating sides,
`/proc/loadavg` 17–24 throughout, release profile (`opt-level = 3`, `lto = "thin"` — the profile the
crate ships). Median of the per-run means (`best` = median of the runs' best-of-20 000, ns/send):

| shape (`clean` destination) | before mean / best | after mean / best | Δ mean | Δ best |
|---|---|---|---|---|
| small tuple (4 words) | 57.4 / 39 | 55.0 / 37 | −4.2 % | −5.1 % |
| 10-int list (20 words) | 89.9 / 71 | 90.7 / 72 | +0.9 % (noise) | — |
| 8 × 32-byte binaries (72 words) | 250.0 / 178 | 228.6 / 170 | −8.6 % | −4.5 % |
| HAMT map, 64 keys (234 words) | 714.5 / 605 | 658.7 / 579 | −7.8 % | −4.3 % |
| **`send_recv` payload (70 words)** | **234.5 / 188** | **210.4 / 181** | **−10.3 %** | **−3.7 %** |
| `send_recv` payload, min over 6 pairs | 220.9 / 186 | 205.9 / 181 | −6.8 % | −2.7 % |

Every one of the 6 interleaved payload pairs moved the same way (all `after` means below all `before`
means), the three-run shapes agree on direction except the 10-int list, which is flat — the win scales
with the number of copied objects, which is what the change removes work per. Deterministic counters are
**unchanged** by it (2 blocks / 192 B per copy for the payload, 0 for the enqueue half — §6's work-stack
would have taken that to 0 but was slower, above).

**In per-op terms:** −24 ns on a ≈ 1 943 ns/op `send_recv_plain` round trip is ≈ 1 % of that workload, so
this is a copy-path win, not a messaging-workload win; the workload's own residual is `beam_rt`'s (§5).
