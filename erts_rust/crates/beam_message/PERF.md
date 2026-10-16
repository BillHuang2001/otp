# `beam_message` — Measured: per-send copy cost (single-pass vs the gate-locked two-traversal path)

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

The `send_recv` workload is **54.7×** the system-ERTS oracle — 79 518.12 ns/op (default tier) /
36 969 (interpreter) vs 1 453.98 (`erts_rust/tools/bench/RESULTS.md` §8, lines ≈ 247 / 291 / 477 /
492). The table above shows the `beam_message` copy share of that is **≈ 350–400 ns/op, i.e. ≈ 0.5 %**
of 79.5 µs (the exact payload's `send` path, including the off-heap drain and the `Message` build).
Even a free copy would leave the workload at ~79 µs/op. **The residual is not in this crate.** Its
sources are, with exact paths:

* **`beam_process` — the delivery path.** `Process::copy_message`
  (`erts_rust/crates/beam_process/src/process.rs` ~line 1528, the dirty young-heap destination above)
  plus `Process::deliver` / `deliver_message` / `run_rooted_collection` and the mailbox + cursor
  selective-receive scan in the same file. This is the largest single component and the natural next
  target.
* **`beam_rt` — the runner/scheduler glue.** The single-threaded `ProcessRunner`/scheduler loop and
  the mailbox bridge under `erts_rust/crates/beam_rt/src/` that drive `beam_process` for the
  `send`/`receive` in the workload.
* **`beam_codec` — the ETF codec.** The `{SR}` case of `erts_rust/tools/bench/src/workloads/copy_bound.rs`
  round-trips the payload through `term_to_binary`/`binary_to_term` (crates/beam_codec), which is
  outside the copy path entirely and may well dominate the measured per-op time.

**Suggested measurement for the parent to hand to those nodes:** re-run the bench driver's
`copy_bound` `send_recv` workload against this crate (the JIT-tier regression noted in
`erts_rust/tools/bench/RESULTS.md` §8 item 2 is a separate Rust-vs-Rust tier effect, unrelated), and
instrument the `{SR}` case to time `term_to_binary`/`binary_to_term` separately from the send/receive
round-trip so the `beam_codec` and `beam_process`/`beam_rt` shares can be split. A `beam_process`
micro-benchmark mirroring `tests/send_perf.rs` (a payload sent to a second process and back, without
the ETF round-trip) would isolate the delivery/scheduler cost from this crate's copy cost.

## 6. Follow-up (this milestone): the fix-up worklist is allocation-free
The copy path's per-call `Vec<Fixup>` worklist is replaced by one shared `WorkStack` (`src/copy.rs`)
that keeps its first `WORK_INLINE = 32` fix-ups in a `MaybeUninit` array **inside the value** and
spills to a `Vec` only past that. `push`/`pop` is the whole API (no traversal logic moved), the
public signatures are unchanged, and the two preflight/error walks (`bounded_words`, `is_acyclic`)
keep their own `Vec<Eterm>` scratch deliberately: they are the *sizing/refusal* path, not the
per-message write path, and neither runs on a copy that fits.
### 6.1 Inline capacity and the spill threshold
* `WORK_INLINE = 32` fix-ups (32 × 16 B = 512 B of frame; `[MaybeUninit::uninit(); 32]` stores
  nothing, so the frame costs a stack-pointer adjustment, not 512 B of stores).
* Rationale: worklist **depth is pending fan-out**, not graph size — one entry per non-shared child
  of the materialised object, i.e. the children left along the current descent path. The whole
  `copy_bound` `send_recv` payload (70 words, 27 objects) leaves **at most 6** entries live; the
  widest single object the frozen layout builds, a HAMT node's 16-slot head array, still leaves room
  to descend inside 32.
* Spill threshold (measured, `tests/counters.rs` §2b: a K-element tuple of compound children = K
  pending fix-ups): every K ≤ 32 copies with **0** allocations; K = 33 (the first arity past the
  capacity) pays **exactly one** 1024 B allocation (= 64 entries × 16 B, the `reserve(2 ·
  WORK_INLINE)` on the first spill) and one dealloc; the bracket "exactly one allocation up to twice
  the capacity (K ≤ 64)" is asserted, and the capacity itself is *derived* from behaviour
  (`first allocating K − 1`), never restated, so a capacity change moves the transition and the
  assertions follow.
### 6.2 Before → after: allocation events per message (exact, `tests/counters.rs`)
Deterministic, `#[global_allocator]`-counted, identical over all 8 repeats per cell. Before = the
per-call `Vec<Fixup>` (commit `3cfcc8c57b`); after = this change. Same four shapes, same
destinations (`size_of_copy(term) + 64` words, empty off-heap table → the single-pass path).
| shape | words/objects | BEFORE `copy_struct` | AFTER `copy_struct` | BEFORE `send` (warm queue) | AFTER `send` (warm queue) |
|---|---|---|---|---|---|
| small tuple | 4 / 1 | 0 allocs, 0 B | **0**, 0 B | 0, 0 B | **0**, 0 B |
| 10-int list | 20 / 10 | 1, 64 B (1 dealloc) | **0**, 0 B | 1, 64 B | **0**, 0 B |
| HAMT map (64 keys) | 234 / 85 | 4, 960 B (4 deallocs) | **0**, 0 B | 4, 960 B | **0**, 0 B |
| `send_recv` payload | 70 / 27 | 2, 192 B (2 deallocs) | **0**, 0 B | 2, 192 B | **0**, 0 B |
A **cold** mailbox costs one extra allocation per mailbox (480 B, the `VecDeque`'s first buffer) on
both sides — once per mailbox, not per message. The enqueue half (off-heap drain + `Message` build +
`Queue::enqueue`) was and stays 0 allocations. So the copy path now asks the allocator for nothing at
all per message.
### 6.3 Wall clock: the measured outcome is *not* a win on every shape
A/B, `tests/perf.rs` (`deep list (2000 cons cells)` = 4000 words, 2000 pushes — the push-heaviest
shape) and `tests/send_perf.rs`, alternating sides within a round, one process at a time, dev profile
(`opt-level=1`), 1-minute `/proc/loadavg` **43–58** over the runs (the host is shared with two other
perf agents, so only `best_ns` — the minimum — is meaningful):
| run | `deep list` best_ns BEFORE | `deep list` best_ns AFTER | `HAMT (64 keys)` best_ns BEFORE | AFTER | `send_recv payload` clean best_ns BEFORE | AFTER |
|---|---|---|---|---|---|---|
| r1 (base first) | 15 138 | 20 275 | 1 252 | 1 163 | 402 | 479 |
| r2 (base first) | 15 889 | 23 865 | 1 170 | 1 315 | 418 | 478 |
| r3 (base first) | 10 587 | 23 849 | 1 081 | 1 315 | 432 | 469 |
| r1 (new first) | 14 929 | 18 877 | — | — | 410 | 383 |
| r2 (new first) | 15 880 | 23 051 | — | — | 356 | 488 |
The reversed-order rounds rule out a warm-up/order bias for the one shape that separates: the
2000-cons copy is **≈ 1.3×–2.2× slower** with the inline stack, consistently, in 5/5 paired rounds.
The HAMT and `send_recv` rows **overlap between the two sides** (e.g. HAMT 1 081–1 252 before vs
1 163–1 315 after; `send_recv` 402–432 vs 383–488): no attributable movement for a real message.
**Verdict, stated plainly:** the change removes 2 allocations (`send_recv` payload) to 4 (HAMT) per
message deterministically, and that is the reason it is kept — but it is **not** a throughput win at
`opt-level=1`, and on the artificial 2000-cons single-message shape it is a measurable *regression*
(a per-push fixed cost, ~+1.3×–2.2×). The wall-clock effect on every message-shaped payload is
inside this host's noise. A variant that also moves the inline entries into the `Vec` on the first
spill (two compares per push) measured the same way earlier at ≈ +10–19 % on the same shape, so the
one-compare encoding was kept as the cheaper of the two.
### 6.4 Verdicts on the other priority items (evidence, not assumption)
* **Redundant copies — none found.** `beam_process::Process::copy_message` passes a *fresh empty*
  `OffHeapTable`, so `copy_struct` always takes `try_copy_fast` (single traversal, one `alloc_words`
  commit); `tests/counters.rs` asserts the `used()` advance equals `size_of_copy(term)` for every
  shape, so no word is written twice, and a repeated subterm is copied twice only by design (the C's
  flat `copy_struct`; the sharing-preserving variant is `copy_struct_shared`).
* **Per-item word overhead — the frozen layout's, and nothing more.** Payload census: 70 words / 27
  objects (21 cons + 6 headed), 8.6 % header words, 2.59 words/object, `max` 8 words; the copier
  adds no per-object tag/boxing word. The old 192 B / 2 events for the payload was `Vec` **capacity**
  (4 then 8 entries for ≥ 32 entries of head-room), now zero.
* **Double traversal — absent on the fast path, gated on the slow one.** The second traversal exists
  only for a destination with a non-empty off-heap table (see §1/§3); the measured real send path
  never takes it.
* **Heap reservation granularity — one commit per copy.** `TrialTarget` reserves per object against
  `[to.hp(), to.hp() + to.available())` and the copy ends in a single `to.alloc_words(written)`, so
  the destination heap sees one reservation per message; the `WorkStack` spill is the only other
  reservation and never fires for a message-sized graph.
