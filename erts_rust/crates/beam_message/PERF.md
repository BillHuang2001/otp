# `beam_message` — Measured: the per-message copy and messaging cost

The measurement record for this crate's share of the process copy boundary: one `send` — `copy_struct` + the
destination off-heap drain + the `Message` build + `Queue::enqueue` — for the shapes
`tests/{counters,send_perf,perf}.rs` build.
The **primary signal is deterministic counters** (§2): they are profile- and load-independent and reproduce exactly.
Every wall-clock figure (§3 release, §4 dev, §6–§7 release A/Bs) is a ratio on a **labelled** regime, never an
absolute to quote.
Every assertion in these harnesses is correctness-only — the enqueued payload is `compare::eq` the original, exactly
one message is enqueued, and `dst.used()` advances by exactly `size_of_copy(term)`; **no test asserts a timing
threshold**.

## 1. Regimes, host and method

**Profiles — never mix them.**

| regime | what it is | sections |
|---|---|---|
| **dev** | plain `cargo test` — crate `opt-level = 1`, deps `opt-level = 2` | §4 |
| **release** | `--release` — `opt-level = 3`, `lto = "thin"`, `codegen-units = 1` (the profile the crate ships) | §3, §6, §7 |

**Host:** shared 14-core sandbox, no isolation; cpu0–3 are 4.7 GHz P-cores and cpu4–15 are 3.3–3.7 GHz E-cores, so an
unpinned run lands on either core type.
Every release figure here is `taskset -c 3`.
`/proc/loadavg` is recorded before and after every run; this session's 1-minute loads were **8.0–16.7** for the dev
runs and **28.6–36.4** for the release runs (the host is shared, so the load is part of the record, not noise to hide).

**Method.**

* `tests/send_perf.rs` — five shapes, each built once on a source heap and then sent `ITERATIONS` times (20 000; the
  64-key HAMT 5 000) through the **real** `beam_message::send`. Both destinations are given identical free head-room
  (`size_of_copy(term) + HEADROOM`, `HEADROOM = 64` words) and a fresh empty staging `OffHeapTable`: `clean` = a
  brand-new heap (`used() == 0`), `dirty` = the same capacity with one word pre-charged. Reports mean and `best`
  (minimum single-send) ns.
* `tests/perf.rs` — `copy_struct` alone, `best-of-100 000`, plus the single-pass-vs-sized preflight probe.
* `tests/counters.rs` — four shapes, three counted phases (bare `copy_struct`; `send` into a **cold** queue; `send` into
  a **warm** queue), each measured `REPEATS = 8` times and asserted **identical**. A counting `#[global_allocator]`
  over `System` supplies allocations/bytes; a graph walk supplies objects and header overhead; `Instant` supplies only
  the copy-vs-enqueue split.

Timing is `std::time::Instant` (no `criterion`, no new dependency); a mean over an iteration batch is load-sensitive, so
`best` is always reported beside it.

## 2. Deterministic counters (the primary signal)

Per-message work for a flat copy into a clean `words + 64`-word heap — `tests/counters.rs`, identical in dev and release
builds:

| shape | words | objects | cons cells | headed | header words | header % | words/object | bytes touched |
|---|---|---|---|---|---|---|---|---|
| small tuple | 4 | 1 | 0 | 1 | 1 | 25.0 % | 4.00 | 64 |
| 10-int list | 20 | 10 | 10 | 0 | 0 | 0.0 % | 2.00 | 320 |
| HAMT map (64 keys) | 234 | 85 | 64 | 21 | 21 | 9.0 % | 2.75 | 3 744 |
| **`send_recv` payload** | **70** | **27** | **21** | **6** | **6** | **8.6 %** | **2.59** | **1 120** |

`words` is `size_of_copy(term)` (= `size_object`), cross-checked against the destination's `used()` advance.
`bytes touched = 2 · words · 8 B` (each source word read once, each destination word written once); its precondition is
asserted — every destination starts with a **fully empty** off-heap table, which is exactly
`copy.rs::is_fast_path_eligible`, so the copier takes its single-pass path and never reads the graph twice.
A destination whose off-heap table is non-empty is the one case this figure would understate.

Allocations per message — the counting `#[global_allocator]`, `allocs / bytes`:

| shape | `copy_struct` | `send` (warm queue) | enqueue half = warm send − copy | `send` (cold queue) |
|---|---|---|---|---|
| small tuple | 0 / 0 B | 0 / 0 B | **0 / 0 B** | 1 / 480 B |
| 10-int list | 1 / 64 B | 1 / 64 B | **0 / 0 B** | 2 / 544 B |
| HAMT map (64 keys) | 4 / 960 B | 4 / 960 B | **0 / 0 B** | 5 / 1 440 B |
| **`send_recv` payload** | **2 / 192 B** | **2 / 192 B** | **0 / 0 B** | 3 / 672 B |

**Reading:**

* **The enqueue half allocates nothing**: warm `send` minus the bare copy is `0` allocations / `0` bytes for every
  shape, so the `Message` build and the FIFO push are allocation-free.
  The cold-queue `send` is exactly one allocation larger than the copy (480 B, identical for every shape) — that is the
  queue's **first buffer**, paid once per mailbox, never per message.
* **The copy's allocations are the fix-up worklist `Vec<Fixup>` and nothing else** (§6). A `Fixup` is 16 B
  (`(*mut Eterm, Eterm)`), so the event counts are the `Vec`'s capacity doublings: 0 for the small tuple (no non-shared
  children), 1 × 64 B (cap 4) for the 10-int list, 2 × 64 + 128 B = **192 B (cap 4 → 8) for the `send_recv` payload**,
  4 × 64 + 128 + 256 + 512 B = 960 B (cap 4 → 32) for the 64-key HAMT.
* The payload's per-message cost is therefore **2 blocks / 192 B, all of it the worklist**; it moves only if the worklist
  does, not with load, profile or shape-independent tuning.
* The 1 120 B touched for the payload is 70 words: 42 cons-cell words (21 × 2, header-less) + 22 headed payload words +
  6 header words; header overhead is 8.6 %, max object 8 words.

## 3. Wall clock — release (the shipping profile)

`send_perf`, `taskset -c 3`, 5 runs at `/proc/loadavg` 29.5–36.4; per shape the **median of the 5 runs' means** and the
**best** single send seen (the two spiked small-tuple entries, 165.0 and 63.4 ns mean, came from the runs at load 34.6 —
the load's, not the code's):

| shape (clean destination) | words | objects | median mean ns/send | best ns/send | ns/word |
|---|---|---|---|---|---|
| small tuple | 4 | 1 | 51.9 | 42 | 13.0 |
| 10-int list | 20 | 10 | 85.1 | 74 | 4.3 |
| 8 × 32-byte binaries | 72 | 16 | 194.9 | 178 | 2.7 |
| HAMT map (64 keys) | 234 | 85 | 559.3 | 477 | 2.4 |
| **`send_recv` payload** | **70** | **27** | **198.8** | **176** | **2.8** |

`objects` is the §2 census where the shape is in `counters`; for the binaries shape it is the structural count
(8 cons + 8 binary objects). `send_perf`'s own printed column carries a hard-coded structural estimate (65 for the HAMT,
26 for the payload) that the census supersedes.

* **The whole `send` for the `send_recv` payload is ≈ 199 ns mean, 176 ns best** — 5 runs spanned 196.4–205.5 ns mean
  and 176–179 ns best, so **quote ≈ 200 ns**, with that run-to-run band as its uncertainty.
  The §7 A/B's 210.4 ns mean / 181 ns best is the same figure from a different session at a lower load.
* Clean and dirty destinations are the **same path** and the same cost: `dirty − clean` stays within ±3 % on every
  70-word-and-up shape and changes sign run to run, and only the 4-word tuple swings wider (−57…−0.4 ns, on a ≈ 50 ns
  absolute cost). That is noise, because the single-pass gate is the off-heap table's emptiness, not the heap's
  `used()`.
* The copy-vs-enqueue split (`counters` under `--release`, one run at `taskset -c 3`, loadavg 28.6): `copy_struct`
  **167.2 ns mean / 149 best**, whole `send` **190.7 / 173**, enqueue phase **23.5 ns** — i.e. ≈ 88 % of a `send` is the
  copy traversal itself and ≈ 12 % the `Message` build + enqueue.
* Scaling between shapes is what the work is: 13.0 ns/word for the fixed-cost-dominated 4-word tuple falls to 2.4–2.8
  ns/word for the 70–234-word graphs.

## 4. Wall clock — dev profile (`perf` and `send_perf` under `cargo test`)

A dev-profile absolute is **not** a shipping figure: the same binary's payload `send` read 290 / 339 / 399 / 424 ns
mean across 4 runs of the same build at loadavg 8.1–16.7. Use these for their **ratios and stability only**.

`tests/perf.rs`, `copy_struct` alone, 4 runs at loadavg 8.1–16.7 (`best_ns` is the least load-sensitive column; the
`mean` over the runs in parentheses):

| shape | words | best ns/copy (range) | mean ns/copy (range) | ns/word |
|---|---|---|---|---|
| small tuple | 4 | 39–40 | 41.6–42.3 (one 135.7 spike) | 10.4 |
| deep list (2000 cons cells) | 4 000 | 7 417–7 447 | 8 521–8 902 (one 11 988 spike) | 2.1–2.2 |
| flat map (8 keys) | 20 | 46–48 | 49.7–53.8 | 2.6 |
| HAMT map (64 keys) | 234 | 648–673 | 700.7–750.7 | 3.0 |
| refc binary reuse (512 B) | 8 | — | 36.1–42.3 | 4.5–5.3 (payload shared, not deep-copied) |

`tests/send_perf.rs`, per shape the range over 4 runs (clean destination; the dirty row tracks it):

| shape | words | mean ns/send (range) | best ns/send (range) |
|---|---|---|---|
| small tuple | 4 | 80.7–107.4 | 51–87 |
| 10-int list | 20 | 112.1–169.9 | 90–149 |
| 8 × 32-byte binaries | 72 | 272.1–350.9 | 225–312 |
| HAMT map (64 keys) | 234 | 770.8–1 274.5 | 697–1 222 |
| `send_recv` payload | 70 | 289.6–424.2 | 244–367 |

**Preflight (Pass 1) share of a small copy** (`tests/perf.rs`, `[single-pass]` = empty destination off-heap table vs
`[sized]` = a non-empty one, which forces the two-traversal path): the sized row costs more in every run but one, whose
negative reading (−21 ns at loadavg 16.7) marks the measurement's floor.
On `best_ns` the small tuple is 51–52 ns sized against 38–40 ns single-pass (+12–14 ns, ≈ +30 %); the flat map is
77–119 ns sized against a 76 ns single-pass, so its share is at or below this host's noise in dev.
The share is **directional evidence, not a pinned percentage** — quote it only with its profile.

## 5. This crate's share of `send_recv_plain`

`copy_bound/send_recv_plain` (rust vs system OTP 29.1.1/erts-17.1): **1 918.8 ns vs 649.4 ns per op, ratio 2.95×**
(`../tools/bench/RESULTS.md`, m15) — the sibling runner's own measurement of the same case is 1 943 ns (median 2 046) vs
666.5 ns, 2.92× (`../beam_rt/src/runner/CONTEXT.md` *Measured*).
`copy_bound/send_recv` (`+` the ETF codec in the worker) is **5.26×** (7 696.8 vs 1 464.3 ns/op, m15); that pair isolates
`beam_codec`'s component inside the two-process path, not this crate's.

**The workload performs two payload sends per op, not one** — `send_recv_plain` sends `Worker ! {self(), T}` and the
worker replies `From ! {self(), T}` — and each send is a 2-tuple (3 words) around the 70-word payload.
At the measured release figures (§3: 198.8 ns for the payload; the 4-word tuple at 51.9 ns gives ≈ 2.2 ns/word of
marginal cost, so a 73-word send is ≈ 206 ns) the two sends are **≈ 0.41 µs/op**:

> **≈ 21 % of the per-op figure is `beam_message`'s `send`** (≈ 0.41 µs of 1 918.8 ns), and this is a **lower bound** —
> `beam_process::deliver` (message registration, receiver wake) and `beam_rt`'s scheduling, receive scan and
> pattern match are outside the measured `send`.

So even a **free** copy would leave the workload at ≈ 1.5 µs/op. The residual is owned by:

* **`beam_rt` — the per-slice runner work.** `../beam_rt/CONTEXT.md` and `src/runner/CONTEXT.md` *Measured* enumerate it:
  the engine moving out of/into a `HashMap` per slice, a fresh `RuntimeContext` + `DispatchMemo` per slice,
  `ctx.enter_module`'s `String`, the park/resume root publish, the receive-timeout timer heap and the per-boundary wake
  sweep. No part of it is a pass sleep: the run route's pass wait is progress-aware and issues **0**
  `clock_nanosleep` on a `send_recv`-shaped run, and `PORT_POLL_SLICE` is a non-destructive zero-timeout probe.
* **`beam_process` — the delivery path.** `Process::copy_message` (the receiver's long-lived, dirty young heap that the
  single-pass gate exists for), `Process::deliver`/`deliver_message`/`run_rooted_collection`, and the mailbox +
  `QueueCursor` selective-receive scan.
* **`beam_codec` — for the `{SR}` case only** (the `term_to_binary`/`binary_to_term` round trip the plain case omits).

**Handoff:** attribution of the remaining ≈ 1.5 µs must come from **counters inside `beam_rt`'s runner** (slices, parks,
wakes, allocations per op), not from this file; `tests/send_perf.rs` is the micro-benchmark that measures this crate's
half of it and is `taskset -c 3`-pinnable.

## 6. The fix-up worklist is a `Vec<Fixup>` — and that is the right call in release

The copy loop's per-call worklist is `Vec<Fixup>` (`src/copy.rs`), 16 B per entry, and §2 shows it is the **only**
per-message allocation: 0 / 1 / 2 / 4 allocation events for the four shapes.

An allocation-free replacement (a `WorkStack`: `[MaybeUninit<Fixup>; WORK_INLINE]` inline plus a `Vec` spill) removes
those events deterministically (counters: **0** per `copy_struct` on every shape but the 64-key HAMT) and does **not**
pay for itself.
Measured in **release** (`opt-level = 3`, `lto = "thin"`, `taskset -c 3`, 6 interleaved pairs, `/proc/loadavg` 17–23,
`WORK_INLINE = 8`): the `send_recv` payload was **neutral** (best-of-20 000 **187 ns vs 186 ns**) and the 10-int list was
**17 % slower** (best 90 ns vs 76 ns).
In **dev** (`opt-level = 1`) the regression is larger and visible on every list-shaped copy (deep list 2000 cons cells
+25 %, 64-key HAMT +11 %) and is a codegen effect of the inlined `pop` round-tripping the fix-up through a stack slot,
not frame size or `MaybeUninit` initialisation (both variants measured).
The `Vec<Fixup>` therefore ships: it is non-regressive in the shipping profile, and the allocation is not where the
per-send cost is.

## 7. The shared-term test runs once per copied object, not twice

A fix-up is only ever recorded for a child already proven **not** shared, so the worklist's pop side calls
`shallow_unshared` while the two root calls keep `shallow`'s `is_shared` guard — one guard (four tag tests) less per
copied object, no added state, no allocation change.

Measured in **release** (`opt-level = 3`, `lto = "thin"`, `taskset -c 3`, alternating sides, `/proc/loadavg` 17–24,
median of the per-run means, `best` = median of the runs' best-of-20 000, ns/send):

| shape (clean destination) | guard on both sides | guard once per object | Δ mean | Δ best |
|---|---|---|---|---|
| small tuple (4 words) | 57.4 / 39 | 55.0 / 37 | −4.2 % | −5.1 % |
| 10-int list (20 words) | 89.9 / 71 | 90.7 / 72 | +0.9 % (noise) | — |
| 8 × 32-byte binaries (72 words) | 250.0 / 178 | 228.6 / 170 | −8.6 % | −4.5 % |
| HAMT map, 64 keys (234 words) | 714.5 / 605 | 658.7 / 579 | −7.8 % | −4.3 % |
| `send_recv` payload (70 words) | 234.5 / 188 | 210.4 / 181 | −10.3 % | −3.7 % |

The saving scales with the number of copied objects, which is exactly the per-object guard it removes; the 10-int list,
one guard per cell on a 2-word object, is flat.
Deterministic counters are unaffected (the payload is still 2 blocks / 192 B in the copy, 0 in the enqueue half), and the
current measurement of the payload `send` (§3: ≈ 199 ns mean / 176 ns best) sits at the 210/181 column's cost.

## 8. Reproduce

```sh
erts_rust/scripts/cargo.sh test -p beam_message --manifest-path erts_rust/Cargo.toml --offline \
    --test counters -- --nocapture
erts_rust/scripts/cargo.sh test -p beam_message --manifest-path erts_rust/Cargo.toml --offline \
    --test perf -- --nocapture
erts_rust/scripts/cargo.sh test -p beam_message --manifest-path erts_rust/Cargo.toml --offline \
    --test send_perf -- --nocapture

# the release regime (the one to quote for a per-send shape):
erts_rust/scripts/cargo.sh build -p beam_message --manifest-path erts_rust/Cargo.toml --offline --release --tests
taskset -c 3 erts_rust/target/release/deps/send_perf-* --nocapture
taskset -c 3 erts_rust/target/release/deps/counters-*  --nocapture
```

Always go through `erts_rust/scripts/cargo.sh` (`$HOME` is read-only) and pass `--manifest-path erts_rust/Cargo.toml`;
never `cd`.
Record `/proc/loadavg` before and after every run and quote it with the figure.
This crate's tests are **all self-checks** — no test in `src/` or `tests/` invokes `erl`/`erlc`; the oracle
cross-checks live in the sibling `../tools/differential/` harness.
