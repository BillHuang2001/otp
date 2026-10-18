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

//! The PCB's **`max_heap_size` policy**: the setting on a real [`Process`], the usage it is charged
//! against, and the verdict the collection/growth doors record — the crate-level half of
//! `erlang:process_flag(max_heap_size, Map)`.
//!
//! The pure truth table (strict `>`, `size == 0`, the flag selection, the charge arithmetic, the
//! floor rule) is unit-tested in `src/max_heap.rs`; this file pins the `Process`-level behaviour:
//!
//! 1. `include_shared_binaries` decides the verdict of a **real** process — the same process and
//!    the same bound answer [`HeapLimitVerdict::Within`] without the flag and
//!    `Kill`/`Log` with it (the oracle's decisive probe: identical workload survives with `false`,
//!    is killed with `true`).
//! 2. The **verdict is recorded at the collection decision**, against the capacity the collection
//!    or growth **installed** — and the collection itself still succeeds: a breach is a latched
//!    notice ([`Process::take_max_heap_verdict`]), never an error and never a termination, because
//!    this crate never terminates anything.
//! 3. The **prospective** rule: a single admission that installs a capacity above the bound is
//!    recorded even though the process's *previous* size was below it — the case a
//!    current-size-only check would silently miss.
//! 4. `kill: false` records [`HeapLimitVerdict::Log`] (log-only; nothing is killed and nothing is
//!    raised), an unlimited setting records nothing, and `set`/`get` round-trips.
//!
//! Determinism: every figure is derived from the public heap-policy arithmetic (the nursery's
//! `capacity_words`, `next_heap_size`) and forced by explicit API calls — no sleep, no wall-clock
//! bound, no oracle. Source terms are built on a plain [`Heap`] arena (the pattern
//! `tests/exhaustion.rs` uses), so a `Process`'s 233-word default nursery never limits a payload.

use beam_alloc::{Heap, RefcBinary};
use beam_common::Result;
use beam_gc::CollectionKind;
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::prelude::{make_small, Eterm};

/// The flags the oracle calls `H_MAX_FLAGS`: kill and log on, shared binaries excluded.
fn killing() -> MaxHeapFlags {
    MaxHeapFlags::default()
}

/// The same flags with `kill: false` — log-only above the bound.
fn logging_only() -> MaxHeapFlags {
    MaxHeapFlags {
        kill: false,
        ..MaxHeapFlags::default()
    }
}

/// A pid with the given number and serial 0 (the process table is not involved in these tests).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A fresh process with the default heap policy.
fn process(number: u64) -> Process {
    Process::new(pid(number)).expect("the default heap allocates")
}

/// A source arena for payload tuples, comfortably larger than a `Process`'s nursery.
fn source() -> Heap {
    Heap::new(4096).expect("the source heap allocates")
}

/// The elements of a tuple occupying exactly `words` heap words (1 header + `words - 1` elements).
fn tuple_elems(words: usize) -> Vec<Eterm> {
    (0..words.saturating_sub(1))
        .map(|i| make_small(i as i64))
        .collect()
}

/// **(1)** `include_shared_binaries` alone decides the verdict of **one** process at **one** bound:
/// the heap capacity is within it, the off-heap payload pushes the charge over it.
///
/// This is the mirror of the oracle probe where the identical workload survives with
/// `include_shared_binaries => false` and is killed with `true`.
#[test]
fn include_shared_binaries_decides_a_real_process_verdict() -> Result<()> {
    let mut process = process(1);
    let payload = vec![0xA5u8; 64 * 1024];
    let binary = RefcBinary::new(&payload)?;
    process.register_off_heap_binary(binary);
    assert_eq!(
        process.off_heap_binary_bytes(),
        payload.len(),
        "the process owns the payload, so its bytes are charged"
    );

    // A bound one word above the heap capacity: the heap alone is within it, and the payload
    // (8192 words) is far above it.
    let heap_words = process.max_heap_usage().heap_words;
    assert!(heap_words > 0, "a live process has a heap capacity");
    let size = heap_words + 1;
    let without = MaxHeapSetting {
        size,
        flags: killing(),
    };
    let with = MaxHeapSetting {
        size,
        flags: MaxHeapFlags {
            include_shared_binaries: true,
            ..killing()
        },
    };

    process.set_max_heap_size(without);
    assert_eq!(
        process.current_heap_limit_verdict(),
        HeapLimitVerdict::Within,
        "the flag is off, so the off-heap payload is not charged"
    );
    assert_eq!(
        process.heap_limit_verdict(heap_words),
        HeapLimitVerdict::Within
    );

    process.set_max_heap_size(with);
    assert_eq!(
        process.current_heap_limit_verdict(),
        HeapLimitVerdict::Kill,
        "with the flag on the same process at the same bound is over it"
    );

    // The same charge with `kill: false` is log-only for the identical process.
    process.set_max_heap_size(MaxHeapSetting {
        size,
        flags: MaxHeapFlags {
            include_shared_binaries: true,
            ..logging_only()
        },
    });
    assert_eq!(
        process.current_heap_limit_verdict(),
        HeapLimitVerdict::Log,
        "kill: false is log-only — nothing is killed and nothing is raised"
    );
    Ok(())
}

/// **(2a)** A collection over the bound records [`HeapLimitVerdict::Kill`] and the collection itself
/// still **succeeds** (its `Result` is untouched), clearable exactly once.
#[test]
fn a_collection_over_the_bound_records_kill_and_still_succeeds() -> Result<()> {
    let mut process = process(2);
    // A bound of one word: any installed capacity is far above it.
    process.set_max_heap_size(MaxHeapSetting {
        size: 1,
        flags: killing(),
    });
    assert_eq!(
        process.max_heap_verdict(),
        None,
        "nothing has been decided before the first collection"
    );

    let collection = process
        .collect()
        .expect("the collection itself still succeeds");
    assert!(matches!(
        collection.kind,
        CollectionKind::Minor | CollectionKind::Major
    ));
    assert!(
        process.max_heap_usage().heap_words > 1,
        "the installed capacity really is above the one-word bound"
    );

    assert_eq!(
        process.max_heap_verdict(),
        Some(HeapLimitVerdict::Kill),
        "the post-collection capacity is over the bound and kill is set"
    );
    assert_eq!(
        process.take_max_heap_verdict(),
        Some(HeapLimitVerdict::Kill),
        "the drain hands the notice over once"
    );
    assert_eq!(
        process.max_heap_verdict(),
        None,
        "and clears it — the next decision re-records if still over"
    );
    Ok(())
}

/// **(2b)** The kind-pinned door (`collect_kind`) records the same verdict as `collect`, so a host
/// that pins the collection kind does not lose the breach notice.
#[test]
fn a_pinned_collection_records_the_verdict_too() -> Result<()> {
    let mut process = process(3);
    process.set_max_heap_size(MaxHeapSetting {
        size: 1,
        flags: killing(),
    });

    process
        .collect_kind(CollectionKind::Minor)
        .expect("the pinned minor succeeds");
    assert_eq!(process.max_heap_verdict(), Some(HeapLimitVerdict::Kill));

    assert_eq!(
        process.take_max_heap_verdict(),
        Some(HeapLimitVerdict::Kill)
    );
    Ok(())
}

/// **(2c)** A growth forced by a `deliver` (the crate's own collect-grow-and-retry path, which
/// drives [`Process::ensure_capacity`]) records the verdict, and the delivery still succeeds.
#[test]
fn a_delivery_that_forces_a_growth_records_the_verdict() -> Result<()> {
    let mut receiver = process(4);
    let mut src = source();

    // Fill the nursery exactly, so the next copy is refused and takes the growth path.
    let capacity = receiver.collector().young().capacity_words();
    let filler = vec![make_small(0); capacity - 1];
    receiver
        .heap_mut()
        .tuple(&filler)
        .expect("the nursery fills exactly");
    assert_eq!(receiver.collector().young().available_words(), 0);

    receiver.set_max_heap_size(MaxHeapSetting {
        size: 1,
        flags: killing(),
    });
    assert_eq!(
        receiver.max_heap_verdict(),
        None,
        "the fill alone decides nothing — a breach is a collection/growth decision"
    );

    // One word larger than the whole nursery: only an on-demand growth can fit it.
    let payload_words = capacity + 1;
    let payload = src
        .tuple(&tuple_elems(payload_words))
        .expect("the payload tuple fits the source heap");
    let outcome = receiver.deliver(pid(5).to_eterm(), payload, &OffHeapTable::new())?;
    assert!(
        matches!(outcome, DeliverOutcome::Enqueued | DeliverOutcome::Woke),
        "the refused delivery is retried once after the growth and enqueued"
    );
    assert!(
        receiver.collector().young().capacity_words() > 1,
        "the growth installed a capacity far above the bound"
    );
    assert_eq!(
        receiver.max_heap_verdict(),
        Some(HeapLimitVerdict::Kill),
        "the growth door recorded the breach"
    );
    Ok(())
}

/// **(3)** The **prospective** rule: a single `ensure_capacity` whose installed capacity is above the
/// bound is recorded, while the process's *previous* heap size was within it.
///
/// This is the finding an occupancy-only check would miss, so it is pinned explicitly: before the
/// call the current verdict is [`HeapLimitVerdict::Within`], after it the recorded one is a breach.
#[test]
fn an_admission_that_installs_a_capacity_above_the_bound_is_recorded() -> Result<()> {
    let mut process = process(6);
    let before = process.max_heap_usage().heap_words;
    assert!(before > 0);
    // The bound is twice the current capacity: comfortably within it *now*.
    process.set_max_heap_size(MaxHeapSetting {
        size: before * 2,
        flags: killing(),
    });
    assert_eq!(
        process.current_heap_limit_verdict(),
        HeapLimitVerdict::Within,
        "the pre-admission size is below the bound — a current-size check would stop here"
    );
    assert_eq!(process.max_heap_verdict(), None);

    // One admission that installs four times the current capacity, i.e. well over the bound.
    let need = before * 4 + 1_000;
    process
        .ensure_capacity(need)
        .expect("the growth is reachable under the unbounded policy");
    assert!(
        process.max_heap_usage().heap_words >= need,
        "the growth really installed a capacity covering the request"
    );
    assert_eq!(
        process.max_heap_verdict(),
        Some(HeapLimitVerdict::Kill),
        "the admission's own prospective capacity is what is judged"
    );
    Ok(())
}

/// **(3b)** The no-relocation growth door (`grow_nursery`, which installs a new young capacity
/// without collecting) records the breach too.
#[test]
fn a_no_relocation_growth_records_the_verdict() -> Result<()> {
    let mut process = process(7);
    let before = process.max_heap_usage().heap_words;
    process.set_max_heap_size(MaxHeapSetting {
        size: before,
        flags: killing(),
    });

    let capacity = process
        .grow_nursery(before * 2)
        .expect("the growth is reachable under the unbounded policy");
    assert!(capacity > before, "the nursery really grew");
    assert_eq!(
        process.max_heap_verdict(),
        Some(HeapLimitVerdict::Kill),
        "a growth that crosses the bound is a breach"
    );
    Ok(())
}

/// **(4)** `kill: false` records [`HeapLimitVerdict::Log`]: the process keeps running and nothing is
/// raised — there is deliberately no third, raising outcome.
#[test]
fn kill_false_records_log_and_nothing_else() -> Result<()> {
    let mut process = process(8);
    process.set_max_heap_size(MaxHeapSetting {
        size: 1,
        flags: logging_only(),
    });

    process.collect().expect("the collection still succeeds");
    assert_eq!(
        process.max_heap_verdict(),
        Some(HeapLimitVerdict::Log),
        "kill: false is log-only, so the verdict is Log and never Kill"
    );
    assert_eq!(process.take_max_heap_verdict(), Some(HeapLimitVerdict::Log));
    Ok(())
}

/// **(4b)** A process comfortably within its bound records nothing at all — the latch only ever
/// holds a breach.
#[test]
fn a_process_within_its_bound_records_nothing() -> Result<()> {
    let mut process = process(9);
    let capacity = process.max_heap_usage().heap_words;
    process.set_max_heap_size(MaxHeapSetting {
        size: capacity * 16,
        flags: killing(),
    });

    process.collect().expect("the collection succeeds");
    process
        .ensure_capacity(capacity)
        .expect("the admission succeeds");
    process.grow_nursery(capacity).expect("the growth succeeds");
    assert_eq!(
        process.max_heap_verdict(),
        None,
        "no door recorded anything: a within-limit answer stores no notice"
    );
    assert_eq!(process.take_max_heap_verdict(), None);
    Ok(())
}

/// **(4c)** An **unlimited** setting records nothing, whatever the usage — `size == 0` is the C's
/// "no upper bound", not a bound of zero.
#[test]
fn an_unlimited_setting_records_nothing() -> Result<()> {
    let mut process = process(10);
    let capacity = process.max_heap_usage().heap_words;
    process.set_max_heap_size(MaxHeapSetting::unlimited(killing()));
    assert!(process.max_heap_size().is_unlimited());

    process.collect().expect("the collection succeeds");
    process
        .ensure_capacity(capacity * 2)
        .expect("the admission succeeds");
    assert_eq!(
        process.max_heap_verdict(),
        None,
        "an unlimited process records no verdict however large it grows"
    );
    assert_eq!(
        process.current_heap_limit_verdict(),
        HeapLimitVerdict::Within
    );

    // And a really large charge is still within an unlimited setting.
    assert_eq!(
        process.heap_limit_verdict(usize::MAX / 2),
        HeapLimitVerdict::Within
    );
    Ok(())
}

/// **(4d)** The setting round-trips through `set`/`get`, replacing the previous bound **and** flags
/// wholesale (the C `erlang:process_flag(max_heap_size, …)` semantics).
#[test]
fn the_setting_round_trips_and_replaces_wholesale() {
    let mut process = process(11);
    let born = process.max_heap_size();
    assert!(born.is_unlimited(), "a process is born unlimited");
    assert_eq!(born.flags, MaxHeapFlags::default());

    let setting = MaxHeapSetting {
        size: 1_000,
        flags: MaxHeapFlags {
            kill: false,
            error_logger: false,
            include_shared_binaries: true,
        },
    };
    process.set_max_heap_size(setting);
    assert_eq!(process.max_heap_size(), setting);

    // Wholesale: the second setting's flags replace the first's entirely.
    let replaced = MaxHeapSetting {
        size: 42,
        flags: MaxHeapFlags::default(),
    };
    process.set_max_heap_size(replaced);
    assert_eq!(process.max_heap_size(), replaced);
    assert!(process.max_heap_size().flags.kill);
    assert!(!process.max_heap_size().flags.include_shared_binaries);

    // Back to unlimited.
    process.set_max_heap_size(MaxHeapSetting::unlimited(MaxHeapFlags::default()));
    assert!(process.max_heap_size().is_unlimited());
}

/// The prospective decision is a **query**: consulting it (or the current form) never records,
/// collects or mutates anything — only a collection/growth door records.
#[test]
fn the_verdict_queries_do_not_record() {
    let mut process = process(12);
    process.set_max_heap_size(MaxHeapSetting {
        size: 1,
        flags: killing(),
    });

    assert_eq!(process.current_heap_limit_verdict(), HeapLimitVerdict::Kill);
    assert_eq!(process.heap_limit_verdict(1 << 20), HeapLimitVerdict::Kill);
    assert_eq!(
        process.max_heap_verdict(),
        None,
        "a query is not a decision point, so it latches nothing"
    );
}
