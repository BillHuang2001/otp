# `beam_process` — message-delivery performance

## Intent

This file records the **process-side** share of the end-to-end `send_recv` workload measured in
`../../tools/bench/RESULTS.md` §3.6/§8, which reports that workload as **one** number: 79 518 ns/op
in the default (native baseline JIT) tier and 36 969 ns/op in the `BEAM_RT_TIER=interpreter` tier,
against the system ERTS's 1 453.98 ns/op (54.7×). Three crates own slices of it:

* **`beam_message`** — the copy engine (`copy_struct` → `send` → `receive`);
* **`beam_engine`** — the `send`/`receive` instruction seams (not reachable from a `beam_process`
  harness; its own figures live in `../beam_engine/MEASURED.md`);
* **this crate** — signal queueing/classification, the slot scan a table delivery pays, the
  run-queue/wake bookkeeping, the per-schedule-in signal servicing, and the selective-receive scan.

`examples/perf_delivery.rs` is the dependency-free instrument that makes this crate's slice
measurable in isolation and attributes the cost between the three. It asserts **only correctness +
structural invariants** — never a wall-clock threshold (this is a shared, loaded host).

## Method

**Host caveat.** Absolute `ns/op` is meaningless on its own here: this is a shared CI/dev host and
the load average during a run is typically in the tens. `/proc/loadavg` is read and printed by the
harness immediately before and after the run, and every row's figure is only meaningful **relative
to the other rows of the same run** — read the ratios, all of which name both of their sides. A
before/after pair is only comparable when the two runs' load averages are close, or when each side
is normalized by a row no change touched (the delivery floor `attr  process.deliver+receive
(immediate)`).

**Command** (sandbox `$HOME` is read-only, so `scripts/cargo.sh` must set a writable `CARGO_HOME`;
never `cd`; never a bare `cargo`):

```text
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release \
    -p beam_process --example perf_delivery -- [scale]
```

`scale` (default `1`) multiplies every row's iteration count.

**Sample discipline.** Every row is **one loop with one divisor**: the row's closure performs
exactly one operation and returns `1` on success, so the row's figure is `elapsed_ns / iterations`.
The harness runs the closure once untimed (warm-up) and then `SAMPLES = 5` timed samples of
`iterations` operations each; the row reports the **median** sample with the **best** sample beside
it so the noise spread is visible. Iteration bases: `ITERS_CHEAP = 1 000 000`,
`ITERS_PAYLOAD = 500 000`, `ITERS_SPAWN = 200 000`.

**Isolation.** The payload rows build the payload fresh on the sender's heap per iteration and
rewind the sender's/receiver's young heap with `Heap::reset_to(mark)` afterwards (the discipline
`../beam_bifs/examples/etf_bench.rs` uses), so a row never runs a collection or a nursery growth —
it measures the code path, not the collector. Receive predicates are cheap
(`payload == make_small(n)`). Every correctness/structure check (the copied payload's shape,
`DeliverOutcome`s, the wake list, the run queue) runs **outside** the timed loop, so timing never
pays for verification.

**The change/control discipline.** The thirteen rows kept across a before/after pair are byte-identical
in the two runs, so a row that moves is the change showing through. Two rows exist as an
**instrument control**: `attr  table.deliver-to-waiting+drain_woken` and
`attr  table.deliver-to-waiting+drain_woken_into (reused buffer)` have the same timed body and
differ only in how the wake list leaves the table, so their **same-run difference** is the
buffer-ownership effect with the delivery, the state transition and the receive cancelled out. In
a build where both rows call the take-based `drain_woken` (every *before* run here), that
difference is the **null control** — it measures the instrument's own noise.

## Baseline (before)

Two consecutive runs of the command above, `scale = 1`, on the pre-change tree. Primary run:

* `loadavg(before) = 38.61 17.29 9.79` · `loadavg(after) = 37.84 17.49 9.89`

| row | ns/op (median) | ns/op (best) |
|---|---:|---:|
| shape a  deliver_simple+receive (immediate) | 31.8 | 31.1 |
| shape b  monitor+demonitor | 24.3 | 24.2 |
| shape b  link+unlink | 8.1 | 8.0 |
| shape b  exit-signal(trapped)+receive | 52.0 | 51.7 |
| shape b  monitor+terminate+DOWN (includes spawn) | 952.8 | 948.4 |
| attr  process.deliver+receive (immediate) | 31.0 | 30.9 |
| attr  process.deliver+receive (copy payload) | 298.0 | 295.3 |
| attr  table.lookup (get + get_mut) | 2.2 | 2.2 |
| attr  table.deliver+receive (immediate) | 34.1 | 33.6 |
| attr  table.deliver-to-waiting+drain_woken | 43.0 | 42.2 |
| attr  scheduler.resume_woken (waiting receiver) | 49.0 | 48.6 |
| attr  scheduler.wake+run_slice (waiting receiver) | 65.6 | 64.9 |
| attr  scan.match (0 non-matching first) | 33.7 | 33.4 |
| attr  scan.match (8 non-matching first) | 34.0 | 33.2 |
| attr  scan.match (32 non-matching first) | 36.8 | 36.6 |

Second run (variance cross-check; `loadavg(before) = 37.84 17.49 9.89`,
`loadavg(after) = 35.21 17.28 9.86`) reports the same rows within noise — e.g. shape a 32.7,
`process.deliver+receive (immediate)` 32.0, `(copy payload)` 299.7, `table.lookup` 2.4,
`table.deliver+receive` 35.1, `table.deliver-to-waiting+drain_woken` 39.3,
`scheduler.resume_woken` 44.2, `scheduler.wake+run_slice` 59.4, scan(0/8/32) 32.0/32.6/37.1.

The rows added since measure the per-schedule-in signal servicing and the reused-buffer drain. Their
**before** figures (the same row, same instrument, on the pre-change tree) are
`attr  table.service_signals (empty queue)` **10.9 ns/op** (loadavg 39.45/37.25) and
**9.9 ns/op** (loadavg 14.73/14.99); `attr  table.service_signals (1 pending exit signal)`
**60.1 / 52.8 ns/op**; and the reused-buffer row at **37.9 / 35.5 ns/op** against its take-based
sibling's **38.1 / 35.5 ns/op** — i.e. a **0.2 ns / 0.0 ns** control delta.

### Ratios (both sides named), before

| ratio | value | lhs | rhs |
|---|---:|---|---|
| table-deliver ÷ process-deliver | **1.100×** | `attr table.deliver+receive (immediate)` 34.1 ns | `attr process.deliver+receive (immediate)` 31.0 ns |
| copy-payload ÷ immediate-payload | **9.603×** | `attr process.deliver+receive (copy payload)` 298.0 ns | `attr process.deliver+receive (immediate)` 31.0 ns |
| waiting-receiver ÷ runnable-receiver | **1.261×** | `attr table.deliver-to-waiting+drain_woken` 43.0 ns | `attr table.deliver+receive (immediate)` 34.1 ns |
| run-slice ÷ resume-woken | **1.341×** | `attr scheduler.wake+run_slice` 65.6 ns | `attr scheduler.resume_woken` 49.0 ns |
| scan(32) ÷ scan(0) | **1.093×** | `attr scan.match (32 non-matching first)` 36.8 ns | `attr scan.match (0 non-matching first)` 33.7 ns |
| shape-a ÷ delivery-floor | **1.024×** | `shape a deliver_simple+receive (immediate)` 31.8 ns | `attr process.deliver+receive (immediate)` 31.0 ns |

(The second run: 1.096× / 9.367× / 1.121× / 1.344× / 1.160× / 1.021× — same shape.)

## After

Three runs of the same command on the current tree. The third is load-matched to the fourth *before*
run above (14.7 vs 14.0), so that pair is the headline comparison; the other two are the variance
cross-check.

| run | `loadavg(before)` | `loadavg(after)` |
|---|---|---|
| A | `44.80 28.62 16.71` | `41.93 28.30 16.67` |
| B | `31.91 31.35 19.05` | `29.76 30.92 18.98` |
| C (load-matched pair) | `14.03 26.11 18.07` | `13.15 25.72 17.98` |

| row | before (run 4, loadavg 14.73/14.99) | after (run C, loadavg 14.03/13.15) | after ÷ before |
|---|---:|---:|---:|
| shape a  deliver_simple+receive (immediate) | 29.8 | 31.1 | 1.044× |
| shape b  monitor+demonitor | 22.7 | 23.4 | 1.031× |
| shape b  link+unlink | 7.3 | 7.5 | 1.027× |
| shape b  exit-signal(trapped)+receive | 49.7 | 51.9 | 1.044× |
| shape b  monitor+terminate+DOWN (includes spawn) | 932.2 | 958.0 | 1.028× |
| attr  process.deliver+receive (immediate) | 27.3 | 29.1 | 1.066× |
| attr  process.deliver+receive (copy payload) | 243.0 | 248.5 | 1.023× |
| attr  table.lookup (get + get_mut) | 1.9 | 2.0 | 1.053× |
| attr  table.deliver+receive (immediate) | 28.8 | 31.0 | 1.076× |
| attr  table.deliver-to-waiting+drain_woken | 35.5 | 36.6 | 1.031× |
| attr  table.deliver-to-waiting+drain_woken_into (reused buffer) | 35.5 | 32.9 | **0.927×** |
| attr  table.service_signals (empty queue) | 9.9 | **1.1** | **0.111×** |
| attr  table.service_signals (1 pending exit signal) | 52.8 | 59.7 | 1.131× |
| attr  scheduler.resume_woken (waiting receiver) | 41.6 | 41.4 | 0.995× |
| attr  scheduler.wake+run_slice (waiting receiver) | 52.7 | **45.7** | **0.867×** |
| attr  scan.match (0 non-matching first) | 29.7 | 30.5 | 1.027× |
| attr  scan.match (8 non-matching first) | 30.3 | 30.8 | 1.017× |
| attr  scan.match (32 non-matching first) | 33.9 | 34.5 | 1.018× |

Run C's `attr  process.deliver+receive (immediate)` (the delivery floor) is **6.6 % above** the
matched before run's, and the rows **no change touched** moved by the same +2.7 %…+4.4 % — the host
was that much slower during run C. Dividing each row by its own run's delivery floor removes that
ambient drift; the floor-normalized movement is then: `service_signals (empty queue)` **0.363 →
0.038**, `wake+run_slice ÷ floor` **1.930 → 1.570** (i.e. the row that carries the schedule-in
plus the runner call moved **−18.6 %**), `drain_woken_into (reused buffer) ÷ floor` **1.300 →
1.130**, `drain_woken ÷ floor` 1.300 → 1.258, `resume_woken ÷ floor` 1.524 → 1.423,
`table.deliver+receive ÷ floor` 1.055 → 1.065, `copy payload ÷ floor` 8.901 → 8.540.

The rows' after figures in the other two runs (A: loadavg 44.80/41.93, B: 31.91/29.76) are
`service_signals (empty queue)` **1.4 / 1.1 ns/op**, `service_signals (1 pending exit signal)`
**75.7 / 62.1 ns/op**, `drain_woken` **46.1 / 38.2 ns/op**, `drain_woken_into (reused buffer)`
**41.5 / 34.9 ns/op**, `resume_woken` **49.5 / 43.4 ns/op**, `wake+run_slice` **53.7 / 48.3 ns/op**,
`table.deliver+receive` **38.6 / 31.6 ns/op**, `process.deliver+receive` (floor) **36.5 / 30.3 ns/op**,
`(copy payload)` **324.0 / 264.6 ns/op**.

### The three changes the instrument proves

1. **Per-schedule-in signal servicing** (`ProcessTable::service_signals`,
   `crate/src/ptab.rs`). A schedule-in of a process whose signal queue is empty — every slice of
   every process — now returns from a single `head_signal` probe instead of seeding a per-call
   `VecDeque` worklist: **10.9 → 1.1 ns/op** (load-matched pair; 10.6 → 1.4 in the loaded pair),
   i.e. **−90 % of the row**, carried through to the production scheduler path by
   `run-slice − resume-woken` **11.1 → 4.3 ns/op** and `scheduler.wake+run_slice` **−18.6 %**
   floor-normalized.
2. **Wake-list buffer ownership** (`ProcessTable::drain_woken_into`, `Scheduler`'s `woken`
   scratch). The table's wake list is moved into a caller-owned buffer with `Vec::append`, which
   leaves both buffers' capacity intact, so neither the `ready.push` nor the sweep allocates after
   the warm-up iteration. Same-run, delivery-cancelled evidence:
   **before 35.5 vs 35.5 (Δ 0.0 ns) and 38.1 vs 37.9 (Δ 0.2 ns) — the control; after 36.6 vs 32.9
   (Δ 3.7 ns) and 46.1 vs 41.5 (Δ 4.6 ns)**. The production `resume_woken` row does **not** resolve
   this change (41.6 → 41.4 is inside its own run-to-run spread); the step-level rows do.
3. **One slot scan per `ProcessTable::deliver`** (the recipient PCB borrow and the collection-gate
   read come from the disjoint `slots`/`collection_gate` fields). The harness cannot resolve it:
   `table.deliver+receive (immediate)` moved 28.8 → 31.0 while its run's floor moved 27.3 → 29.1, so
   floor-normalized it is **1.055 → 1.065** — inside the ±1 ns jitter of a row whose noise band is
   wider than the expected saving. The saving is bounded above by the `table.lookup (get + get_mut)`
   row (**1.9–2.5 ns/op for two scans**, so ≈1 ns for the one scan removed): a strict work
   reduction, not a measured win.

The `service_signals (1 pending exit signal)` row moves **+13 %** (52.8 → 59.7 raw, 1.934 → 2.052
floor-normalized): the quiet-path probe that the fast path adds is paid once more by the
queue-non-empty path (which then enters the worklist). The trade is deliberate — that path runs when
a signal is actually waiting, while the quiet path runs on **every** slice of **every** process.

### Ratios (both sides named), after

Run C (load-matched to run 4), floor = `attr  process.deliver+receive (immediate)` 29.1 ns:

| ratio | value | lhs | rhs |
|---|---:|---|---|
| table-deliver ÷ process-deliver | **1.065×** | `attr table.deliver+receive (immediate)` 31.0 ns | `attr process.deliver+receive (immediate)` 29.1 ns |
| copy-payload ÷ immediate-payload | **8.540×** | `attr process.deliver+receive (copy payload)` 248.5 ns | `attr process.deliver+receive (immediate)` 29.1 ns |
| wait+take-drain ÷ runnable-receiver | **1.258×** | `attr table.deliver-to-waiting+drain_woken` 36.6 ns | `attr table.deliver+receive (immediate)` 31.0 ns |
| wait+reused-drain ÷ runnable-receiver | **1.130×** | `attr table.deliver-to-waiting+drain_woken_into (reused buffer)` 32.9 ns | `attr table.deliver+receive (immediate)` 31.0 ns |
| run-slice ÷ resume-woken | **1.104×** | `attr scheduler.wake+run_slice` 45.7 ns | `attr scheduler.resume_woken` 41.4 ns |
| service_signals(pending) ÷ service_signals(empty) | **54.017×** | `attr table.service_signals (1 pending exit signal)` 59.7 ns | `attr table.service_signals (empty queue)` 1.1 ns |
| scan(32) ÷ scan(0) | **1.109×** | `attr scan.match (32 non-matching first)` 34.5 ns | `attr scan.match (0 non-matching first)` 30.5 ns |
| shape-a ÷ delivery-floor | **1.069×** | `shape a deliver_simple+receive (immediate)` 31.1 ns | `attr process.deliver+receive (immediate)` 29.1 ns |

### Differences (owner-attributed; both sides named), after

| difference | value | lhs | rhs | owner |
|---|---:|---|---|---|
| copy-payload − immediate-payload | **219.4 ns/op** | copy-payload 248.5 ns | immediate-payload 29.1 ns | `beam_message` (`copy_struct`/`send`) |
| wake+drain(take) − wake+drain(reused buffer) | **3.7 ns/op** | `table.deliver-to-waiting+drain_woken` 36.6 ns | `…drain_woken_into (reused buffer)` 32.9 ns | `beam_process` (`ptab.rs` wake-list buffer ownership) |
| wake+drain(take) − no-wake | **5.6 ns/op** | `table.deliver-to-waiting+drain_woken` 36.6 ns | `table.deliver+receive (immediate)` 31.0 ns | `beam_process` (`ptab.rs` `ready.push` + `drain_woken`) |
| table-deliver − process-deliver | **1.9 ns/op** | `table.deliver+receive (immediate)` 31.0 ns | `process.deliver+receive (immediate)` 29.1 ns | `beam_process` (`ptab.rs` slot scan + gate + wake record) |
| run-slice − resume-woken | **4.3 ns/op** | `scheduler.wake+run_slice` 45.7 ns | `scheduler.resume_woken` 41.4 ns | `beam_process` (`scheduler.rs` schedule-in/out + `service_signals`) |
| one isolated slot scan | **2.0 ns/op** | `attr table.lookup (get + get_mut)` 2.0 ns | — (one `ptab.rs` `lookup`/`lookup_mut`) | `beam_process` (`ptab.rs`) |
| per-candidate receive-scan predicate | **≤0.13 ns/candidate** | `scan(32)` 34.5 ns | `scan(0)` 30.5 ns | `beam_process`'s `receive` + the matching `pred` |

## Attribution

The three candidate owners and the raw numbers that separate them, in this crate's own units (run C,
loadavg 14.03/13.15):

* **`beam_message` — the copy engine.** `copy-payload ÷ immediate-payload = 8.540×`
  (248.5 ns vs 29.1 ns); the difference is **219.4 ns/op**, i.e. ~88 % of the copy row and the
  single largest cost on the delivery path. The immediate-payload row writes **no heap word** (the
  floor's assertion is `heap_used_words() == baseline`), so the entire difference is `copy_struct`
  walking the 5-key map (10-element list, `{ok, <<"hello world">>, 3.5}` tuple, 12-byte binary,
  char-list, `-42`) into the receiver's young heap. **Not this crate's.**
* **`beam_process` — table classification, wake/drain, the schedule-in and the scheduler slice.**
  `table-deliver − process-deliver = 1.9 ns/op` (`ptab.rs`'s single slot scan + the
  `declines_signal_collection` gate + the wake record); `wake+drain − no-wake = 5.6 ns/op`, of which
  `3.7 ns/op` is the take-based drain's buffer ownership (the two drain rows' same-run difference)
  and the rest is the `ready.push` itself; `run-slice − resume-woken = 4.3 ns/op` (`scheduler.rs`'s
  schedule-in/out plus the `service_signals` above); one isolated slot scan is 2.0 ns/op, and the
  marginal selective-receive candidate is ≤0.13 ns/candidate.
* **`beam_engine` — the instruction seams.** Not reachable from a `beam_process` harness: the
  `send`/`receive` instructions live above this crate and this crate never touches a `Machine`. Its
  share of `send_recv` is measured in its own node (`../beam_engine/MEASURED.md`) and is the
  remainder of the end-to-end figure once the copy and process sides are subtracted; the engine's
  own side of a BIF call is bounded at ~11–12 ns/call (`../beam_engine/tests/bif_cost.rs`).

### The end-to-end `send_recv` split

One `send_recv` op is **one full two-process round trip** (`../../tools/bench/src/workloads/copy_bound.rs`):
the sender builds the payload, sends `{self(), T}` to a worker, the worker replies
`{self(), binary_to_term(term_to_binary(T))}`, and the sender matches the reply against the bound
`T` (a deep structural comparison). The payload crosses the process boundary **twice** per op, at
the harness's own row cost (run C, same payload as the benchmark:

```text
(i)   beam_process   ≈ 2 × (delivery floor 29.1 + table scan/gate/wake 1.9 + slice marginal 4.3
                          + schedule-in 1.1)                ≈  73 ns/op  ≈ 0.09 % of 79 518 ns/op
(ii)  beam_message   ≈ 2 × 219.4                            ≈ 439 ns/op  ≈ 0.55 % of 79 518 ns/op
(iii) remainder      ≈ 79 518 − 73 − 439 = 79 006 ns/op     ≈ 99.4 %
```

The **remainder** is everything the workload executes above these two crate boundaries: the
`beam_engine` `send`/`receive` instruction seams plus the Erlang code itself (the payload is
constructed on **both** sides, the worker's `term_to_binary`+`binary_to_term` pair is measured by the
sibling `etf_roundtrip` case at 9 838 ns/op for the identical payload = 12.4 % of this op, the
sender's deep `case T2 of T` comparison, the list/loop arithmetic) and the `beam_rt` host seams it
runs on. This harness cannot decompose that residual — it never enters a `Machine` — so it is
labelled as beyond the three crates rather than attributed inside them.

The interpreter tier puts the whole figure lower (36 969 ns/op), so the same two rows are **1.19 %
(`beam_message`) and 0.20 % (`beam_process`)** of it. Read against the end-to-end workload, this
crate's Phase-B changes are worth ≈8.8 ns per schedule-in and ≈3.7 ns per non-empty wake sweep — of
the order of **25 ns/op, ≈0.03 %** of the 79 518 ns/op `send_recv` figure. The crate-side changes are
real and measurable **per delivery**; the end-to-end workload is dominated by work outside this
crate, so no end-to-end movement is claimed.

**Read:** the copy engine dominates the delivery path (≈219 of ≈248 ns, ~88 %); this crate's own
share is small — the `Process::deliver`/`Process::receive` floor itself is ≈29 ns/op, the table
classification ≈2 ns, the wake/drain ≈6 ns and the schedule-in ≈4 ns — and concentrated in
`ptab.rs` (slot scan, wake list, signal servicing) and `scheduler.rs` (the slice).

## What this file does not measure

* `beam_engine`'s send/receive instruction cost (unreachable from this crate — see its own node).
* Port delivery, cross-scheduler (SMP) wake/steal, and distribution-driven signals: this harness is
  single-threaded and local, matching the workload the end-to-end `send_recv` exercises.
* Collections: the heap is rewound per iteration, so no row pays for a GC.
* The end-to-end `send_recv` decomposition as a *measurement*: the three-way split above is the
  harness's per-crate rows applied to the workload's structure, not a profiled end-to-end trace.

## See Also

* `examples/perf_delivery.rs` — the harness.
* `../../tools/bench/RESULTS.md` §3.6/§8 — the end-to-end `send_recv` figures this file splits.
* `../beam_engine/MEASURED.md` — the engine node's own measurements; `../beam_message/CONTEXT.md`
  — the copy engine.
