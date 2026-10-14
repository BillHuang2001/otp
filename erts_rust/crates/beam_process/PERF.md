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
  run-queue/wake bookkeeping, and the selective-receive scan.

`examples/perf_delivery.rs` is the dependency-free instrument that makes this crate's slice
measurable in isolation and attributes the cost between the three. It asserts **only correctness +
structural invariants** — never a wall-clock threshold (this is a shared, loaded host).

## Method

**Host caveat.** Absolute `ns/op` is meaningless on its own here: this is a shared CI/dev host and
the load average during a run is typically in the tens. `/proc/loadavg` is read and printed by the
harness immediately before and after the run, and every row's figure is only meaningful **relative
to the other rows of the same run** — read the ratios, all of which name both of their sides.

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

## Baseline (before)

Two consecutive runs of the command above, `scale = 1`. Primary run:

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

### Ratios (both sides named)

| ratio | value | lhs | rhs |
|---|---:|---|---|
| table-deliver ÷ process-deliver | **1.100×** | `attr table.deliver+receive (immediate)` 34.1 ns | `attr process.deliver+receive (immediate)` 31.0 ns |
| copy-payload ÷ immediate-payload | **9.603×** | `attr process.deliver+receive (copy payload)` 298.0 ns | `attr process.deliver+receive (immediate)` 31.0 ns |
| waiting-receiver ÷ runnable-receiver | **1.261×** | `attr table.deliver-to-waiting+drain_woken` 43.0 ns | `attr table.deliver+receive (immediate)` 34.1 ns |
| run-slice ÷ resume-woken | **1.341×** | `attr scheduler.wake+run_slice` 65.6 ns | `attr scheduler.resume_woken` 49.0 ns |
| scan(32) ÷ scan(0) | **1.093×** | `attr scan.match (32 non-matching first)` 36.8 ns | `attr scan.match (0 non-matching first)` 33.7 ns |
| shape-a ÷ delivery-floor | **1.024×** | `shape a deliver_simple+receive (immediate)` 31.8 ns | `attr process.deliver+receive (immediate)` 31.0 ns |

(The second run: 1.096× / 9.367× / 1.121× / 1.344× / 1.160× / 1.021× — same shape.)

### Differences (owner-attributed; both sides named)

| difference | value | lhs | rhs | owner |
|---|---:|---|---|---|
| copy-payload − immediate-payload | **266.9 ns/op** | copy-payload 298.0 ns | immediate-payload 31.0 ns | `beam_message` (`copy_struct`/`send`) |
| wake+drain − no-wake | **8.9 ns/op** | `table.deliver-to-waiting+drain_woken` 43.0 ns | `table.deliver+receive (immediate)` 34.1 ns | `beam_process` (`ptab.rs` `ready.push` + `drain_woken`) |
| run-slice − resume-woken | **16.7 ns/op** | `scheduler.wake+run_slice` 65.6 ns | `scheduler.resume_woken` 49.0 ns | `beam_process` (`scheduler.rs` schedule-in/out + `service_signals`) |
| table-deliver − process-deliver | **3.1 ns/op** | `table.deliver+receive (immediate)` 34.1 ns | `process.deliver+receive (immediate)` 31.0 ns | `beam_process` (`ptab.rs` scans + gate + wake record) |
| one isolated slot scan | **2.2 ns/op** | `attr table.lookup (get + get_mut)` 2.2 ns | — (one `ptab.rs` `lookup`/`lookup_mut`) | `beam_process` (`ptab.rs`) |
| per-candidate receive-scan predicate | **0.0–0.1 ns/candidate** | `scan(32)` 36.8 ns | `scan(0)` 33.7 ns | `beam_process`'s `receive` + the matching `pred` |

## Attribution

The three candidate owners and the raw numbers that separate them:

* **`beam_message` — the copy engine.** `copy-payload ÷ immediate-payload = 9.603×`
  (298.0 ns vs 31.0 ns); the difference is **266.9 ns/op**, i.e. ~90 % of the copy row. The
  immediate-payload row writes **no heap word** (the floor's assertion is
  `heap_used_words() == baseline`), so the entire difference is `copy_struct` walking the
  5-key map (10-element list, 12-byte binary, float 3.5, char-list, `-42`) into the receiver's
  young heap. This is the dominant cost on the whole delivery path and is **not** this crate's.
* **`beam_process` — table classification, wake/drain, and the scheduler slice.**
  `table-deliver − process-deliver = 3.1 ns/op` (`ptab.rs`'s two linear `lookup`/`lookup_mut`
  scans of `slots` + the `declines_signal_collection` gate + the wake record);
  `wake+drain − no-wake = 8.9 ns/op` (`ptab.rs` `self.ready.push(to)` at `:862` and
  `drain_woken` at `:889`, whose `core::mem::take` reallocates);
  `run-slice − resume-woken = 16.7 ns/op` (`scheduler.rs` `slice_begin` `:630` / `slice_end`
  `:695` plus the per-schedule-in `service_signals` at `:673-674`). A single isolated slot scan is
  2.2 ns/op, and the marginal selective-receive candidate is 0.0–0.1 ns/candidate.
* **`beam_engine` — the instruction seams.** Not reachable from a `beam_process` harness: the
  `send`/`receive` instructions live above this crate and this crate never touches a `Machine`. Its
  share of `send_recv` is measured in its own node (`../beam_engine/MEASURED.md`) and is the
  remainder of the end-to-end figure once the copy and process sides are subtracted.

**Read:** the copy engine dominates the delivery path (≈267 of ≈298 ns, ~90 %); this crate's own
share is small and concentrated in the wake/drain bookkeeping (`ptab.rs`) and the scheduler slice
(`scheduler.rs`), with the `Process::deliver`/`Process::receive` floor itself at ≈31 ns/op.

## What this file does not measure

* `beam_engine`'s send/receive instruction cost (unreachable from this crate — see its own node).
* Port delivery, cross-scheduler (SMP) wake/steal, and distribution-driven signals: this harness is
  single-threaded and local, matching the workload the end-to-end `send_recv` exercises.
* Collections: the heap is rewound per iteration, so no row pays for a GC.

## See Also

* `examples/perf_delivery.rs` — the harness.
* `../../tools/bench/RESULTS.md` §3.6/§8 — the end-to-end `send_recv` figures this file splits.
* `../beam_engine/MEASURED.md` — the engine node's own measurements; `../beam_message/CONTEXT.md`
  — the copy engine.
