// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! **Micro-benchmark: the process-side message-delivery path (`ns/op`), with the cost
//! attributed between this crate and its two neighbours.**
//!
//! The end-to-end `send_recv` workload (`tools/bench`) measures two processes, a payload copied
//! sender → receiver and round-tripped, as **one** number — 79 518 ns/op in the default
//! (native baseline JIT) tier and 36 969 ns/op in the `BEAM_RT_TIER=interpreter` tier, against
//! the system ERTS's 1 453.98 ns/op. Three crates own slices of that number:
//!
//! * `beam_message` — the copy engine (`copy_struct` → `send` → `receive`);
//! * `beam_engine` — the `send`/`receive` instruction seams (not reachable from this crate);
//! * **this crate** — signal queueing/classification, the slot scan a `ProcessTable` delivery
//!   pays, the run-queue/wake bookkeeping, and the selective-receive scan.
//!
//! This example makes *this* crate's share measurable by measuring the same delivery from three
//! different entry points (a bare [`Process`], the [`ProcessTable`], and a [`Scheduler`] slice)
//! and with three different payloads (an immediate, and the realistic `send_recv` map).
//! It is **std-only and dependency-free** (no `criterion`), prints its table on stdout (an
//! example needs no `--nocapture`) and asserts only **correctness + structural invariants** —
//! never a wall-clock threshold, which would be flaky on a shared, loaded host.
//!
//! # Method
//!
//! Every row is **one loop with one divisor**: the row's closure performs exactly one operation
//! and returns `1` on success, the harness runs it once untimed (warm-up) and then [`SAMPLES`]
//! times [`iterations`](Report::iterations) times, and the row's figure is
//! `elapsed_ns / iterations` of the **median** sample (the best sample is printed beside it so
//! the noise spread is visible). The absolute figure is meaningless on its own — read the
//! **ratios**, and always alongside `/proc/loadavg`, which this harness reads and prints
//! immediately before and after the run.
//!
//! Two deliberate choices keep a row's number about *this code path* rather than about the
//! collector:
//!
//! * the payload is built **fresh on the sender's heap** per iteration and the receiver's /
//!   sender's young heap is rewound with `Heap::reset_to(mark)` after each iteration, so a row
//!   never runs a collection or a nursery growth (exactly the discipline
//!   `beam_bifs/examples/etf_bench.rs` uses); and
//! * the receive predicates are cheap (`payload == make_small(n)`), so a row's own assertion
//!   cost stays in the same order as the work measured.
//!
//! The **correctness** of every shape is asserted outside the timed loop (a structural check of
//! the copied payload, `DeliverOutcome`s, the wake list, the run queue) so the timing samples
//! never pay for the verification.
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release \
//!     -p beam_process --example perf_delivery -- [scale]
//! ```
//!
//! `scale` (default `1`) multiplies every row's iteration count, for a quick run.
//!
//! # Rows
//!
//! | row | what it measures |
//! |---|---|
//! | `shape a  deliver_simple+receive` | the end-to-end shape: an immediate delivered and matched out |
//! | `shape b  monitor+demonitor` | the monitor bookkeeping half of the signal/link/monitor surface |
//! | `shape b  link+unlink` | the link-set half of the same surface |
//! | `shape b  exit-signal(trapped)+receive` | a full exit-signal delivery into a trapping process |
//! | `shape b  monitor+terminate+DOWN` | a target's death resolving a monitor (`noproc`-free path) |
//! | `attr  process.deliver+receive (immediate)` | **the process-side delivery floor**, no copy |
//! | `attr  process.deliver+receive (copy payload)` | the same loop with the `send_recv` payload |
//! | `attr  table.lookup (get + get_mut)` | the linear slot scan of a `ProcessTable`, isolated |
//! | `attr  table.deliver+receive (immediate)` | the floor through `ProcessTable::deliver` |
//! | `attr  table.deliver-to-waiting+drain_woken` | the same with a `Waiting` receiver (a wake) |
//! | `attr  scheduler.resume_woken (waiting receiver)` | the wake list → run queue step |
//! | `attr  scheduler.wake+run_slice (waiting receiver)` | wake + a full slice (schedule-in, runner, out) |
//! | `attr  scan.match (mailbox: 1 message)` | a selective receive over one message |
//! | `attr  scan.match (8 / 32 non-matching first)` | the per-candidate predicate cost |

use std::hint::black_box;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// Shape of the measurement
// ---------------------------------------------------------------------------

/// The number of timed samples per row; the report quotes the **median**.
const SAMPLES: usize = 5;

/// The five map keys of the `send_recv` payload, in the oracle's own order.
const PAYLOAD_KEYS: [&str; 5] = ["a", "b", "c", "d", "e"];

/// The non-matching payload a selective-receive row prefills its mailbox with. A single
/// *distinct* immediate, so it can never equal a row's `make_small(n)` match value.
const FILLER: i64 = -1;

/// The counted shapes' base iteration counts, chosen so one timed sample is a few hundred
/// milliseconds on the reference host. `scale` (argv[1]) multiplies them all.
const ITERS_CHEAP: u64 = 1_000_000;
const ITERS_PAYLOAD: u64 = 500_000;
const ITERS_SPAWN: u64 = 200_000;

// ---------------------------------------------------------------------------
// Fixtures: atoms and the `send_recv` payload
// ---------------------------------------------------------------------------

/// The shared atom table plus the payload's atom indices.
///
/// One table serves the [`SignalAtoms`] (which every [`ProcessTable`] needs), the payload's five
/// map keys and the `ok` atom of the payload's tuple, so a single index space covers a run.
struct Names {
    table: AtomTable,
    keys: [Atom; 5],
    ok: Atom,
}

impl Names {
    fn new() -> Names {
        let mut table = AtomTable::new();
        // The signal atoms come first, so the table's index order is stable across runs.
        let _signals = SignalAtoms::from_table(&mut table);
        let mut keys = [Atom::from_raw(0); 5];
        for (slot, name) in keys.iter_mut().zip(PAYLOAD_KEYS) {
            *slot = table.intern(name);
        }
        let ok = table.intern("ok");
        Names { table, keys, ok }
    }

    /// A fresh [`ProcessTable`] over this atom space.
    fn table(&self) -> ProcessTable {
        let mut table = AtomTable::new();
        ProcessTable::new(SignalAtoms::from_table(&mut table))
    }

    fn key(&self, index: usize) -> Eterm {
        make_atom(self.keys[index])
    }
}

/// Writes `bytes` as a fresh byte-aligned binary on `heap`.
fn heap_binary(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let words = heap_bits_size_words(bytes.len() * 8);
    // SAFETY: `heap` is an exclusively-owned bump heap; `alloc_words(words)` hands back `words`
    // writable words, which is exactly `write_binary`'s reservation for a binary this large.
    let hp = unsafe { heap.alloc_words(words) }.expect("the binary fits the sender's heap");
    // SAFETY: as above — `hp` heads exactly `words` writable words.
    unsafe { write_binary(hp, bytes) }.expect("a byte-aligned binary is buildable")
}

/// Writes the float `value` as a fresh boxed flonum on `heap`.
fn heap_float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `heap` is an exclusively-owned bump heap and `FLOAT_SIZE_WORDS` is the flonum
    // block's exact size.
    let hp = unsafe { heap.alloc_words(FLOAT_SIZE_WORDS) }.expect("the flonum fits");
    // SAFETY: `hp` heads exactly `FLOAT_SIZE_WORDS` writable words.
    unsafe { write_float(hp, value) }
}

/// Builds the map `pairs` on `heap` with [`beam_term::map::write_map`].
fn heap_map(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    let words = map_words_capacity(pairs.len());
    // SAFETY: `heap` is an exclusively-owned bump heap; `alloc_words(words)` hands back
    // `map_words_capacity(pairs.len())` writable words, which is `write_map`'s reservation.
    let hp = unsafe { heap.alloc_words(words) }.expect("the map fits");
    // SAFETY: as above — `hp` heads exactly `words` writable words.
    unsafe { write_map(hp, pairs, atoms) }.expect("the map is buildable")
}

/// The `send_recv` bench case's `payload/0`, built **fresh on `heap`**:
///
/// ```text
/// #{a => [1, ..., 10],
///   b => {ok, <<"hello world">>, 3.5},
///   c => <<1, ..., 12>>,
///   d => "a char list",
///   e => -42}
/// ```
///
/// The Erlang source of the external bench generates this from the same constants, so the two
/// sides cannot drift.
fn build_payload(heap: &mut Heap, names: &Names) -> Eterm {
    let mut list = NIL;
    for n in (1..=10i64).rev() {
        list = heap.cons(make_small(n), list).expect("the list fits");
    }
    let mut chars = NIL;
    for byte in "a char list".bytes().rev() {
        chars = heap
            .cons(make_small(i64::from(byte)), chars)
            .expect("the char list fits");
    }
    let hello = heap_binary(heap, b"hello world");
    let three_point_five = heap_float(heap, 3.5);
    let tuple = heap
        .tuple(&[make_atom(names.ok), hello, three_point_five])
        .expect("the tuple fits");
    let twelve = heap_binary(heap, &(1..=12u8).collect::<Vec<u8>>());
    heap_map(
        heap,
        &[
            (names.key(0), list),
            (names.key(1), tuple),
            (names.key(2), twelve),
            (names.key(3), chars),
            (names.key(4), make_small(-42)),
        ],
        &names.table,
    )
}

/// Walks a proper list of small integers into a `Vec<i64>`.
fn list_values(list: Eterm) -> Vec<i64> {
    let mut out = Vec::new();
    let mut cursor = list;
    while is_list(cursor) {
        // SAFETY: `is_list(cursor)` just proved `cursor` is a live cons cell, so its header and
        // both of its words are initialised; `car`/`cdr` are total on a list cell.
        let head = unsafe { car(cursor) };
        cursor = unsafe { cdr(cursor) };
        out.push(signed_value(head));
    }
    assert_eq!(cursor, NIL, "a proper list ends in NIL");
    out
}

/// The payload bytes of a byte-aligned on-heap binary term.
fn binary_bytes(term: Eterm) -> Vec<u8> {
    // SAFETY: every term this harness passes here is a live term the copy engine wrote into the
    // receiver's young heap; `is_binary` only reads the object header.
    assert!(unsafe { is_binary(term) }, "expected a binary");
    // SAFETY: `term` is a live binary the copy engine wrote into the receiver's young heap, and
    // `bitstring_parts` reports its base byte, bit offset and bit size.
    let (base, offset, bits) = unsafe { bitstring_parts(term) };
    assert_eq!(offset % 8, 0, "the payload's binaries are byte-aligned");
    let len = bits / 8;
    // SAFETY: `base` is the byte containing bit 0 of the binary and `len` whole bytes follow it.
    let slice = unsafe { core::slice::from_raw_parts(base, len) };
    slice.to_vec()
}

/// Structurally verifies a **copied** payload against the `send_recv` shape — and that it is a
/// distinct heap object from the sender's original (i.e. a real copy, not an alias).
fn verify_payload(copied: Eterm, original: Eterm, names: &Names) {
    assert_ne!(
        copied, original,
        "the receiver's payload must be its own heap object, not the sender's"
    );
    assert_eq!(
        map_size(copied, &names.table).expect("a map"),
        5,
        "the copied payload keeps all five keys"
    );
    for (index, name) in PAYLOAD_KEYS.iter().enumerate() {
        assert!(
            map_get(copied, names.key(index), &names.table)
                .expect("a map")
                .is_some(),
            "the copied payload is missing key `{name}`"
        );
    }
    let value = |index: usize| -> Eterm {
        map_get(copied, names.key(index), &names.table)
            .expect("a map")
            .unwrap_or_else(|| panic!("the copied payload is missing key `{}`", PAYLOAD_KEYS[index]))
    };
    assert_eq!(
        list_values(value(0)),
        (1..=10).collect::<Vec<i64>>(),
        "key `a` is the 10-element list"
    );
    assert_eq!(
        binary_bytes(value(2)),
        (1..=12u8).collect::<Vec<u8>>(),
        "key `c` is the 12-byte binary"
    );
    assert_eq!(
        list_values(value(3)),
        "a char list".bytes().map(i64::from).collect::<Vec<i64>>(),
        "key `d` is the char-list string"
    );
    assert_eq!(
        signed_value(value(4)),
        -42,
        "key `e` is -42"
    );
    let tuple = value(1);
    // SAFETY: `tuple` is the live 3-tuple the copy engine wrote into the receiver's heap (the
    // payload shape built above), so `is_tuple`, its size word and elements 0..3 are in bounds.
    unsafe {
        assert!(is_tuple(tuple), "key `b` is the `{{ok, _, _}}` tuple");
        assert_eq!(tuple_size(tuple), 3);
        assert_eq!(tuple_element(tuple, 0), make_atom(names.ok));
        assert_eq!(binary_bytes(tuple_element(tuple, 1)), b"hello world");
        assert_eq!(float_value(tuple_element(tuple, 2)), 3.5);
    }
}

// ---------------------------------------------------------------------------
// The timing harness
// ---------------------------------------------------------------------------

/// One row's measurement: the per-sample wall-clock totals and the last iteration's flag.
struct Report {
    label: String,
    /// The number of operations per timed sample.
    iterations: u64,
    /// The per-sample wall clock, in sample order.
    samples: Vec<Duration>,
    /// The last iteration's success flag (every row returns `1` when its op behaved).
    ok: u64,
}

impl Report {
    /// Nanoseconds per operation for one sample.
    fn ns_per_op(&self, sample: Duration) -> f64 {
        sample.as_secs_f64() * 1e9 / self.iterations as f64
    }

    /// The fastest sample (the least-noise estimate).
    fn best(&self) -> Duration {
        *self
            .samples
            .iter()
            .min()
            .expect("every row has at least one sample")
    }

    /// The median sample.
    fn median(&self) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    /// The reported `ns/op` (median sample).
    fn median_ns(&self) -> f64 {
        self.ns_per_op(self.median())
    }

    /// The best sample's `ns/op` (the noise floor).
    fn best_ns(&self) -> f64 {
        self.ns_per_op(self.best())
    }
}

/// Warm up once, then time [`SAMPLES`] samples of `iterations` operations each.
fn measure(label: impl Into<String>, iterations: u64, mut run: impl FnMut() -> u64) -> Report {
    assert!(
        iterations > 0,
        "a row must divide its wall clock by a positive operation count"
    );
    let mut ok = run();
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iterations {
            ok = run();
        }
        samples.push(start.elapsed());
    }
    Report {
        label: label.into(),
        iterations,
        samples,
        ok,
    }
}

/// The row with exactly this label (a harness invariant: a missing row is a bug in this file).
fn row<'a>(reports: &'a [Report], label: &str) -> &'a Report {
    reports
        .iter()
        .find(|report| report.label == label)
        .unwrap_or_else(|| {
            panic!(
                "the row `{label}` must be measured, measured: {:?}",
                reports.iter().map(|r| r.label.as_str()).collect::<Vec<_>>()
            )
        })
}

/// The 1-minute load average, as `/proc/loadavg` reports it (or the whole first field).
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|line| line.trim_end().to_string())
        .unwrap_or_else(|_| "unavailable".to_string())
}

// ---------------------------------------------------------------------------
// Shape (a): enqueue → wake → receive
// ---------------------------------------------------------------------------

/// **Shape (a).** An immediate delivered from a sender into a `Runnable` receiver and matched
/// straight back out of its mailbox — the smallest end-to-end delivered-message round.
fn bench_shape_a(iters: u64) -> Report {
    let mut receiver = Process::new(pid(2)).expect("the default heap allocates");
    let from = pid(1).to_eterm();
    let mut n = 0i64;
    let report = measure("shape a  deliver_simple+receive (immediate)", iters, || {
        n += 1;
        let expected = make_small(n);
        let outcome = receiver
            .deliver_simple(from, expected)
            .expect("an immediate always fits");
        let matched = receiver
            .receive(|message| message.payload() == expected)
            .expect("the message just delivered is queued");
        black_box(matched.from());
        u64::from(outcome == DeliverOutcome::Enqueued && matched.payload() == expected)
    });
    assert_eq!(report.ok, 1, "the timed loop's last operation must succeed");
    assert!(
        receiver.mailbox().is_empty(),
        "every delivered message was matched out"
    );
    report
}

// ---------------------------------------------------------------------------
// Shape (b): the signal / link / monitor cascade
// ---------------------------------------------------------------------------

/// **Shape (b.1).** A `monitor` + `demonitor` cycle over one live pair: the reference
/// allocation, both slot scans and the outgoing/incoming entry push-and-remove.
fn bench_monitor_cycle(iters: u64) -> Report {
    let names = Names::new();
    let mut table = names.table();
    let watcher = table.spawn_default().expect("the watcher spawns");
    let target = table.spawn_default().expect("the target spawns");
    let report = measure("shape b  monitor+demonitor", iters, || {
        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both halves are live");
        let removed = table.demonitor(watcher, reference).expect("the watcher is live");
        u64::from(removed)
    });
    assert_eq!(report.ok, 1, "every monitor was demonitored");
    assert!(
        table
            .get(watcher)
            .expect("the watcher is live")
            .monitors()
            .is_empty(),
        "the cycle leaves no outgoing monitor behind"
    );
    report
}

/// **Shape (b.2).** A `link` + `unlink` cycle over one live pair — the link-set half of the same
/// surface (`Links`' `Vec` add/remove plus the two slot lookups).
fn bench_link_cycle(iters: u64) -> Report {
    let names = Names::new();
    let mut table = names.table();
    let a = table.spawn_default().expect("a spawns");
    let b = table.spawn_default().expect("b spawns");
    let report = measure("shape b  link+unlink", iters, || {
        table.link(a, b).expect("both pids are live");
        table.unlink(a, b).expect("both pids are live");
        1
    });
    assert_eq!(report.ok, 1);
    assert!(
        table.get(a).expect("a is live").links().is_empty(),
        "the cycle leaves no link behind"
    );
    report
}

/// **Shape (b.3).** An exit signal applied to a **trapping** process linked to its signaller: the
/// classification (`exit_policy`), the reason copy at enqueue time, the eager service and the
/// `{'EXIT', From, Reason}` message the receiver then matches out.
fn bench_trapped_exit(iters: u64) -> Report {
    let names = Names::new();
    let mut table = names.table();
    let signaller = table.spawn_default().expect("the signaller spawns");
    let trapper = table.spawn_default().expect("the trapper spawns");
    table
        .get_mut(trapper)
        .expect("the trapper is live")
        .set_trap_exit(true);
    table
        .link(signaller, trapper)
        .expect("both pids are live");
    let reason = make_small(11);
    let report = measure("shape b  exit-signal(trapped)+receive", iters, || {
        let effect = table
            .apply_exit_signal(trapper, ExitSignal { from: signaller, reason })
            .expect("the trapper is live");
        let matched = table
            .get_mut(trapper)
            .expect("the trapper is live")
            .receive(|_| true)
            .expect("the trapped EXIT is queued");
        black_box(matched.payload());
        // Rewind the trapper's young heap: the `{'EXIT', …}` tuple the signal built there is
        // dead once the message has been matched out, and the row must not measure a collection.
        table
            .get_mut(trapper)
            .expect("the trapper is live")
            .heap_mut()
            .reset_to(0)
            .expect("the trapper's heap rewinds");
        u64::from(effect == ExitEffect::Trapped)
    });
    assert_eq!(report.ok, 1, "every signal was trapped");
    assert!(
        table.contains(trapper),
        "a trapping receiver survives every trapped exit"
    );
    assert!(
        table.woken().is_empty(),
        "a runnable trapping receiver is never pushed to the wake list"
    );
    report
}

/// **Shape (b.4).** A target's death resolving the watcher's monitor: spawn the target, monitor
/// it, terminate it and match the `{'DOWN', Ref, process, Target, Reason}` out of the watcher's
/// mailbox. Labelled as including the spawn: the row is the *whole* monitor-death cascade, so its
/// figure is not comparable with the mailbox-only rows.
fn bench_monitor_down(iters: u64) -> Report {
    let names = Names::new();
    let mut table = names.table();
    let watcher = table.spawn_default().expect("the watcher spawns");
    let reason = make_small(13);
    let report = measure("shape b  monitor+terminate+DOWN (includes spawn)", iters, || {
        let target = table.spawn_default().expect("the target spawns");
        table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both pids are live");
        table.terminate(target, reason).expect("the target is live");
        let matched = table
            .get_mut(watcher)
            .expect("the watcher is live")
            .receive(|_| true)
            .expect("the DOWN is queued eagerly");
        black_box(matched.payload());
        table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut()
            .reset_to(0)
            .expect("the watcher's heap rewinds");
        1
    });
    assert_eq!(report.ok, 1);
    assert_eq!(
        table.len(),
        1,
        "every iteration's target was terminated, so only the watcher is left"
    );
    assert!(
        table
            .get(watcher)
            .expect("the watcher is live")
            .mailbox()
            .is_empty(),
        "every DOWN was matched out"
    );
    assert!(
        table
            .get(watcher)
            .expect("the watcher is live")
            .monitors()
            .is_empty(),
        "every monitor was resolved by its target's death"
    );
    report
}
// ---------------------------------------------------------------------------
// Attribution: the three entry points and the three payloads
// ---------------------------------------------------------------------------

/// **The process-side delivery floor.** An immediate delivered with [`Process::deliver`] (the
/// off-heap-aware form, with an empty source table) into an already-`Runnable` receiver, then
/// matched out. This is the bookkeeping floor with **no** copy-engine work: `copy_struct` of an
/// immediate writes no heap word, and the receiver's heap is asserted to stay empty.
fn bench_process_deliver_immediate(iters: u64) -> Report {
    let mut receiver = Process::new(pid(3)).expect("the default heap allocates");
    let from = pid(4).to_eterm();
    let empty = OffHeapTable::new();
    let baseline = receiver.heap_used_words();
    let mut n = 0i64;
    let report = measure(
        "attr  process.deliver+receive (immediate)",
        iters,
        || {
            n += 1;
            let expected = make_small(n);
            let outcome = receiver
                .deliver(from, expected, &empty)
                .expect("an immediate always fits");
            let matched = receiver
                .receive(|message| message.payload() == expected)
                .expect("the message just delivered is queued");
            u64::from(outcome == DeliverOutcome::Enqueued && matched.payload() == expected)
        },
    );
    assert_eq!(report.ok, 1);
    assert_eq!(
        receiver.heap_used_words(),
        baseline,
        "an immediate payload copies no heap word into the receiver"
    );
    report
}

/// **The copy-engine share.** Exactly the floor's loop, but the payload is the realistic
/// `send_recv` map, built fresh on the sender's heap every iteration (and both heaps rewound, so
/// the row measures the copy engine, not a collection). The floor's `ns/op` subtracted from this
/// row's is the `beam_message` share.
fn bench_process_deliver_payload(iters: u64, names: &Names) -> Report {
    let mut sender = Process::new(pid(5)).expect("the default heap allocates");
    let mut receiver = Process::new(pid(6)).expect("the default heap allocates");
    let from = sender.pid().to_eterm();
    let empty = OffHeapTable::new();
    let report = measure(
        "attr  process.deliver+receive (copy payload)",
        iters,
        || {
            sender
                .heap_mut()
                .reset_to(0)
                .expect("the sender's heap rewinds");
            let payload = build_payload(sender.heap_mut(), names);
            let outcome = receiver
                .deliver(from, payload, &empty)
                .expect("the payload fits the receiver's nursery");
            let matched = receiver
                .receive(|_| true)
                .expect("the message just delivered is queued");
            black_box(matched.payload());
            receiver
                .heap_mut()
                .reset_to(0)
                .expect("the receiver's heap rewinds");
            u64::from(outcome == DeliverOutcome::Enqueued)
        },
    );
    assert_eq!(report.ok, 1);

    // One untimed round, structurally verified: the copy really is the `send_recv` payload.
    sender
        .heap_mut()
        .reset_to(0)
        .expect("the sender's heap rewinds");
    let payload = build_payload(sender.heap_mut(), names);
    receiver
        .deliver(from, payload, &empty)
        .expect("the payload fits the receiver's nursery");
    let message = receiver
        .receive(|_| true)
        .expect("the verification message is queued");
    verify_payload(message.payload(), payload, names);
    report
}

/// **The table's classification share, isolated.** One `get` + one `get_mut` on a live slot: the
/// two linear scans `ProcessTable::deliver` pays per operation, with no delivery, wake or copy
/// work around them.
fn bench_table_lookup(iters: u64, names: &Names) -> Report {
    let mut table = names.table();
    let to = table.spawn_default().expect("the receiver spawns");
    let report = measure("attr  table.lookup (get + get_mut)", iters, || {
        // `black_box` the pid (and the answers): a slot lookup has no side effect, so without an
        // opaque input the optimiser folds the whole loop away and the row would report ~0 ns.
        let slot = black_box(to);
        let read = table.get(black_box(slot)).is_some();
        let write = table.get_mut(black_box(slot)).is_some();
        u64::from(black_box(read) && black_box(write))
    });
    assert_eq!(report.ok, 1);
    report
}

/// **The floor through the table.** The same immediate + receive loop as
/// [`bench_process_deliver_immediate`], with [`ProcessTable::deliver`] in place of
/// [`Process::deliver`] and a second slot lookup for the receive. The floor's `ns/op` subtracted
/// from this row's is the `ptab.rs` classification cost (two scans + the collection-gate check +
/// the wake record), and [`bench_table_lookup`]'s `ns/op` is the third scan's.
fn bench_table_deliver_immediate(iters: u64, names: &Names) -> Report {
    let mut table = names.table();
    let to = table.spawn_default().expect("the receiver spawns");
    let from = pid(7).to_eterm();
    let empty = OffHeapTable::new();
    let mut n = 0i64;
    let report = measure(
        "attr  table.deliver+receive (immediate)",
        iters,
        || {
            n += 1;
            let expected = make_small(n);
            let outcome = table
                .deliver(to, from, expected, &empty)
                .expect("the receiver is live");
            let matched = table
                .get_mut(to)
                .expect("the receiver is live")
                .receive(|message| message.payload() == expected)
                .expect("the message just delivered is queued");
            u64::from(outcome == DeliverOutcome::Enqueued && matched.payload() == expected)
        },
    );
    assert_eq!(report.ok, 1);
    assert!(
        table.woken().is_empty(),
        "a runnable receiver never lands on the wake list"
    );
    report
}

/// **The wake / drain share.** The table loop against a **`Waiting`** receiver: the delivery
/// reports [`DeliverOutcome::Woke`] and records the pid on the table's wake list, which
/// [`ProcessTable::drain_woken`] then takes (the `core::mem::take` reallocation). Subtracting
/// [`bench_table_deliver_immediate`]'s `ns/op` isolates the wake record + drain.
fn bench_table_deliver_waiting(iters: u64, names: &Names) -> Report {
    let mut table = names.table();
    let to = table.spawn_default().expect("the receiver spawns");
    let from = pid(8).to_eterm();
    let empty = OffHeapTable::new();
    let mut n = 0i64;
    let report = measure(
        "attr  table.deliver-to-waiting+drain_woken",
        iters,
        || {
            n += 1;
            let expected = make_small(n);
            table
                .get_mut(to)
                .expect("the receiver is live")
                .set_state(ProcessState::Waiting);
            let outcome = table
                .deliver(to, from, expected, &empty)
                .expect("the receiver is live");
            let woken = table.drain_woken();
            let woke = outcome == DeliverOutcome::Woke && woken.len() == 1 && woken[0] == to;
            let matched = table
                .get_mut(to)
                .expect("the receiver is live")
                .receive(|message| message.payload() == expected)
                .expect("the message just delivered is queued");
            u64::from(woke && matched.payload() == expected)
        },
    );
    assert_eq!(report.ok, 1);
    assert!(
        table.woken().is_empty(),
        "the drain empties the wake list every iteration"
    );
    report
}

/// **The wake → run-queue step.** `Scheduler::resume_woken` over a freshly woken receiver: the
/// wake list drain, the liveness/state test and the run-queue insertion, plus the dequeue the
/// caller must do to keep the queue bounded.
fn bench_scheduler_resume_woken(iters: u64, names: &Names) -> Report {
    let mut table = names.table();
    let to = table.spawn_default().expect("the receiver spawns");
    let from = pid(9).to_eterm();
    let empty = OffHeapTable::new();
    let mut scheduler = Scheduler::new();
    let mut n = 0i64;
    let report = measure(
        "attr  scheduler.resume_woken (waiting receiver)",
        iters,
        || {
            n += 1;
            let expected = make_small(n);
            table
                .get_mut(to)
                .expect("the receiver is live")
                .set_state(ProcessState::Waiting);
            table
                .deliver(to, from, expected, &empty)
                .expect("the receiver is live");
            let resumed = scheduler.resume_woken(&mut table);
            let popped = scheduler.run_queue_mut().dequeue();
            if let Some(pid) = popped {
                // The scheduler's schedule-in clears the marker when it starts a slice; a bare
                // dequeue must clear it too, or the next iteration's `resume_woken` would take
                // the pid for already-queued and skip it.
                table
                    .get_mut(pid)
                    .expect("the dequeued pid is live")
                    .flags_mut()
                    .set_in_run_queue(false);
            }
            let matched = table
                .get_mut(to)
                .expect("the receiver is live")
                .receive(|message| message.payload() == expected)
                .expect("the message just delivered is queued");
            u64::from(resumed == 1 && popped == Some(to) && matched.payload() == expected)
        },
    );
    assert_eq!(report.ok, 1);
    assert!(
        scheduler.run_queue().is_empty(),
        "every woken process was dequeued again"
    );
    report
}

/// A stand-in runner for the scheduler rows: it matches one message out of the process's mailbox
/// (the matched `receive` a real engine would perform) and then blocks.
struct BlockingRunner {
    /// How many slices actually found and consumed a message.
    consumed: u64,
}

impl ProcessRunner for BlockingRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> beam_common::Result<RunOutcome> {
        if let Some(process) = table.get_mut(pid) {
            if process.receive(|_| true).is_some() {
                self.consumed += 1;
            }
        }
        Ok(RunOutcome::Wait)
    }
}

/// **The whole scheduler round.** A `Waiting` receiver woken by the delivery, absorbed into the
/// run queue, and then run through one [`Scheduler::run_slice`]: the schedule-in (the process
/// state transition **and the per-schedule-in `service_signals`**), one runner call and the
/// schedule-out. Subtracting [`bench_scheduler_resume_woken`]'s `ns/op` isolates the slice.
fn bench_scheduler_wake_run_slice(iters: u64, names: &Names) -> Report {
    let mut table = names.table();
    let to = table.spawn_default().expect("the receiver spawns");
    let from = pid(10).to_eterm();
    let empty = OffHeapTable::new();
    let mut scheduler = Scheduler::new();
    let mut runner = BlockingRunner { consumed: 0 };
    let mut n = 0i64;
    let report = measure(
        "attr  scheduler.wake+run_slice (waiting receiver)",
        iters,
        || {
            n += 1;
            let expected = make_small(n);
            table
                .get_mut(to)
                .expect("the receiver is live")
                .set_state(ProcessState::Waiting);
            let outcome = table
                .deliver(to, from, expected, &empty)
                .expect("the receiver is live");
            let resumed = scheduler.resume_woken(&mut table);
            let ran = scheduler
                .run_slice(&mut table, &mut runner)
                .expect("the slice runs");
            u64::from(
                outcome == DeliverOutcome::Woke && resumed == 1 && ran == Some(to),
            )
        },
    );
    assert_eq!(report.ok, 1);
    assert_eq!(
        runner.consumed,
        iters * SAMPLES as u64 + 1,
        "every slice's runner matched one delivered message (plus the untimed warm-up)"
    );
    assert!(
        scheduler.run_queue().is_empty(),
        "a blocking runner leaves the run queue empty"
    );
    report
}

/// **The selective-receive scan.** The mailbox is prefilled once with `k` non-matching messages;
/// each iteration then delivers one matching message at the tail and scans `k + 1` candidates to
/// match it out, so the mailbox returns to exactly `k` messages. `k = 0` is the one-message case,
/// and `(scan(k) - scan(0)) / k` is the per-candidate predicate cost.
fn bench_scan(nonmatching: usize, iters: u64) -> Report {
    let mut receiver = Process::new(pid(11)).expect("the default heap allocates");
    let from = pid(12).to_eterm();
    for _ in 0..nonmatching {
        receiver
            .deliver_simple(from, make_small(FILLER))
            .expect("an immediate always fits");
    }
    assert_eq!(receiver.mailbox().len(), nonmatching);
    let mut n = 0i64;
    let report = measure(
        format!("attr  scan.match ({nonmatching} non-matching first)"),
        iters,
        || {
            n += 1;
            let expected = make_small(n);
            receiver
                .deliver_simple(from, expected)
                .expect("an immediate always fits");
            let matched = receiver
                .receive(|message| message.payload() == expected)
                .expect("the tail message is the match");
            u64::from(matched.payload() == expected)
        },
    );
    assert_eq!(report.ok, 1);
    assert_eq!(
        receiver.mailbox().len(),
        nonmatching,
        "only the matching message is removed, so the scan position resets every iteration"
    );
    report
}

/// A pid with the given number and serial `0` (the crate's `ProcessId::new`).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Print the measurement table (row, iterations, `ns/op` for the median and best samples).
fn report_table(reports: &[Report]) {
    println!(
        "perf_delivery | beam_process message delivery — ns/op (median of {SAMPLES} samples; \
         best sample beside it)"
    );
    println!(
        "perf_delivery | {:<52} {:>10} {:>12} {:>12}",
        "row", "iters", "ns/op", "ns/op(best)"
    );
    for report in reports {
        println!(
            "perf_delivery | {:<52} {:>10} {:>12.1} {:>12.1}",
            report.label,
            report.iterations,
            report.median_ns(),
            report.best_ns(),
        );
    }
    println!();
}

/// Print one ratio with **both sides named**: the short ratio, then the two rows it divides.
fn report_ratio(short: &str, left: &Report, right: &Report) {
    println!(
        "perf_delivery |   {short:<45} = {:>8.3}x",
        left.median_ns() / right.median_ns()
    );
    println!(
        "perf_delivery |       lhs  {:<52} = {:>10.1} ns/op",
        left.label,
        left.median_ns()
    );
    println!(
        "perf_delivery |       rhs  {:<52} = {:>10.1} ns/op",
        right.label,
        right.median_ns()
    );
}

/// Print one difference with **both sides named** and the owner it attributes to.
fn report_share(short: &str, left: &Report, right: &Report, owner: &str) {
    println!(
        "perf_delivery |   {short:<45} = {:>8.1} ns/op   (owner: {owner})",
        left.median_ns() - right.median_ns()
    );
    println!(
        "perf_delivery |       lhs  {:<52} = {:>10.1} ns/op",
        left.label,
        left.median_ns()
    );
    println!(
        "perf_delivery |       rhs  {:<52} = {:>10.1} ns/op",
        right.label,
        right.median_ns()
    );
}

/// Print the attribution: the ratios and differences that separate the three crates' shares.
fn report_attribution(reports: &[Report]) {
    let floor = row(reports, "attr  process.deliver+receive (immediate)");
    let payload = row(reports, "attr  process.deliver+receive (copy payload)");
    let lookup = row(reports, "attr  table.lookup (get + get_mut)");
    let table_plain = row(reports, "attr  table.deliver+receive (immediate)");
    let table_waiting = row(reports, "attr  table.deliver-to-waiting+drain_woken");
    let resume = row(reports, "attr  scheduler.resume_woken (waiting receiver)");
    let slice = row(reports, "attr  scheduler.wake+run_slice (waiting receiver)");
    let scan0 = row(reports, "attr  scan.match (0 non-matching first)");
    let scan8 = row(reports, "attr  scan.match (8 non-matching first)");
    let scan32 = row(reports, "attr  scan.match (32 non-matching first)");

    println!(
        "perf_delivery | attribution — every figure names both of its sides; the ratios are the \
         signal, the absolutes are host-dependent"
    );
    println!();
    report_ratio("table-deliver ÷ process-deliver", table_plain, floor);
    report_ratio("copy-payload ÷ immediate-payload", payload, floor);
    report_ratio(
        "waiting-receiver ÷ runnable-receiver",
        table_waiting,
        table_plain,
    );
    report_ratio("run-slice ÷ resume-woken", slice, resume);
    report_ratio("scan(32) ÷ scan(0)", scan32, scan0);
    report_ratio(
        "shape-a ÷ delivery-floor",
        row(reports, "shape a  deliver_simple+receive (immediate)"),
        floor,
    );
    println!();
    report_share(
        "copy-payload − immediate-payload",
        payload,
        floor,
        "beam_message (`copy_struct`/`send`)",
    );
    report_share(
        "table-deliver − process-deliver",
        table_plain,
        floor,
        "beam_process (`ptab.rs` scans + gate + wake record)",
    );
    report_share(
        "wake+drain − no-wake",
        table_waiting,
        table_plain,
        "beam_process (`ptab.rs` `ready.push` + `drain_woken`)",
    );
    report_share(
        "run-slice − resume-woken",
        slice,
        resume,
        "beam_process (`scheduler.rs` schedule-in/out + `service_signals`)",
    );
    println!();
    let scan_slope = |k: usize, report: &Report| (report.median_ns() - scan0.median_ns()) / k as f64;
    println!(
        "perf_delivery |   per-candidate receive-scan predicate         = {:>8.1} ns/candidate (8) \
         / {:>8.1} ns/candidate (32)   (owner: beam_process's `receive` + the matching `pred`)",
        scan_slope(8, scan8),
        scan_slope(32, scan32),
    );
    println!(
        "perf_delivery |       lhs  {:<52} = {:>10.1} ns/op",
        scan32.label,
        scan32.median_ns()
    );
    println!(
        "perf_delivery |       rhs  {:<52} = {:>10.1} ns/op",
        scan0.label,
        scan0.median_ns()
    );
    println!(
        "perf_delivery |   of which: one isolated slot scan (1 of the 3 `table.deliver` pays) \
         = {:>8.1} ns/op   (owner: beam_process `ptab.rs`)",
        lookup.median_ns(),
    );
    println!();
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let scale: u64 = std::env::args()
        .nth(1)
        .map(|arg| arg.parse().expect("scale must be an unsigned integer"))
        .unwrap_or(1);
    assert!(scale > 0, "scale must be positive");

    let before = loadavg();
    println!("perf_delivery | loadavg(before) = {before}");
    println!(
        "perf_delivery | scale = {scale}  (base iterations x {scale})  samples = {SAMPLES}  \
         crate = beam_process {}",
        env!("CARGO_PKG_VERSION"),
    );
    println!();

    let names = Names::new();
    let cheap = ITERS_CHEAP * scale;
    let payload_iters = ITERS_PAYLOAD * scale;

    // Every row runs in one sequential list, so no two rows contend for the timed cores.
    let reports = vec![
        bench_shape_a(cheap),
        bench_monitor_cycle(cheap),
        bench_link_cycle(cheap),
        bench_trapped_exit(cheap),
        bench_monitor_down(ITERS_SPAWN * scale),
        bench_process_deliver_immediate(cheap),
        bench_process_deliver_payload(payload_iters, &names),
        bench_table_lookup(cheap, &names),
        bench_table_deliver_immediate(cheap, &names),
        bench_table_deliver_waiting(cheap, &names),
        bench_scheduler_resume_woken(cheap, &names),
        bench_scheduler_wake_run_slice(payload_iters, &names),
        bench_scan(0, cheap),
        bench_scan(8, cheap),
        bench_scan(32, payload_iters),
    ];

    report_table(&reports);
    report_attribution(&reports);

    let after = loadavg();
    println!("perf_delivery | loadavg(after) = {after}");
    println!();
    println!(
        "perf_delivery | {:<52} {:>12}",
        "row (checksum repeated)", "ns/op"
    );
    for report in &reports {
        println!(
            "perf_delivery | {:<52} {:>12.1}",
            report.label,
            report.median_ns()
        );
    }

    // Structural invariants only — never a performance threshold (that would be flaky on a
    // shared, loaded host; the printed numbers are the deliverable).
    //
    // Every row asserts its own correctness while it runs; this block is the *harness*
    // invariant: the labelled set of rows must be complete (the table/attribution loops print
    // only what they find, so a row quietly dropped above would otherwise go unnoticed) and
    // every figure must be a plausible positive finite duration.
    for label in [
        "shape a  deliver_simple+receive (immediate)",
        "shape b  monitor+demonitor",
        "shape b  link+unlink",
        "shape b  exit-signal(trapped)+receive",
        "shape b  monitor+terminate+DOWN (includes spawn)",
        "attr  process.deliver+receive (immediate)",
        "attr  process.deliver+receive (copy payload)",
        "attr  table.lookup (get + get_mut)",
        "attr  table.deliver+receive (immediate)",
        "attr  table.deliver-to-waiting+drain_woken",
        "attr  scheduler.resume_woken (waiting receiver)",
        "attr  scheduler.wake+run_slice (waiting receiver)",
        "attr  scan.match (0 non-matching first)",
        "attr  scan.match (8 non-matching first)",
        "attr  scan.match (32 non-matching first)",
    ] {
        assert!(
            reports.iter().any(|report| report.label == label),
            "the row `{label}` must be measured, measured: {:?}",
            reports.iter().map(|r| r.label.as_str()).collect::<Vec<_>>()
        );
    }
    for report in &reports {
        let ns = report.median_ns();
        assert!(
            ns.is_finite() && ns > 0.0,
            "{} must report a positive finite ns/op, got {ns}",
            report.label
        );
        assert!(
            (1.0..1_000_000.0).contains(&ns),
            "{} reported an implausible {ns} ns/op",
            report.label
        );
        assert_eq!(report.ok, 1, "{}'s last operation must succeed", report.label);
    }

    // The scan rows must be ordered as the scan length grows: a longer prefix of non-matching
    // messages cannot be cheaper than a shorter one (this is a *structural* claim about the
    // mailbox, not a threshold).
    let scan = |k: usize| row(&reports, &format!("attr  scan.match ({k} non-matching first)"));
    assert!(
        scan(0).median_ns() <= scan(8).median_ns() && scan(8).median_ns() <= scan(32).median_ns(),
        "a selective receive over more non-matching messages must not be cheaper: \
         {} / {} / {} ns/op",
        scan(0).median_ns(),
        scan(8).median_ns(),
        scan(32).median_ns(),
    );
}
