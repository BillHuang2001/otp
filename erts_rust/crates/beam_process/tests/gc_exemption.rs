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

//! Integration evidence for the **host-controllable GC relocation exemption**
//! ([`Process::set_relocation_exempt`]).
//!
//! A host can hold live terms from a process's young heap in Rust locals and nested-`Machine`
//! registers that are **not** in the crate root set ([`Process::collect`] publishes only
//! `x[0..live)`, the whole stack, the mailbox payloads, the process dictionary, the saved BIF/NIF
//! args and the pending-signal queue). Relocating the heap against that incomplete root set recycles
//! live words — a wrong result. While a process is marked relocation-exempt:
//!
//! * [`Process::deliver`] surfaces the **raw** `BeamError::Alloc(AllocKind::Heap | AllocKind::Message)`
//!   refusal unchanged — no collection, no growth, no retry — which is exactly the signal a host's
//!   fragment fallback keys on (`beam_rt::context::RuntimeContext::deliver_message` →
//!   `send_fragmented_message`), so no heap word moves under the host's unpublished handles;
//! * [`Process::collect`] / [`Process::ensure_capacity`] refuse with the typed `relocation_exempt`
//!   raise instead of relocating;
//! * [`Process::collect_if_needed`] is a no-op answering `Ok(false)` ("no collection ran").
//!
//! **Discriminating power (the point of this file): these tests FAIL if `deliver` collects / grows
//! despite the exemption.** They assert that neither the minor-GC counter nor the nursery base
//! pointer changes across a refused delivery, and that a Rust-local term into the nursery is still
//! intact afterwards — all three break the moment the exemption short-circuit is removed (the
//! collect-and-grow path relocates the nursery and recycles the un-rooted term). The counterpart
//! test pins that the *same* delivery on an **unexempt** process **does** collect and grow, so the
//! exemption is proven to be the cause of the difference, not an incidental refusal.
//!
//! Determinism: every size below is derived from the public heap-policy arithmetic
//! ([`next_heap_size`], `HeapPolicy::from_min_max`); exhaustion is forced by a payload one word
//! larger than the whole nursery, so no timing, sleep or wall-clock bound is involved. The source
//! payloads are built on a plain [`Heap`] arena, so the 233-word default nursery of a `Process`
//! never limits the payload size.

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::next_heap_size;
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm};

/// A pid with the given number and serial 0 (the process table is not involved in the GC tests).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A source arena: every payload tuple and expected tuple is built here, comfortably larger than
/// the 233-word nursery of a fresh `Process`.
fn source() -> Heap {
    Heap::new(4096).expect("the source heap allocates")
}

/// The elements of a tuple occupying exactly `words` heap words (`1` header + `words - 1`
/// elements, per `size_object`). The elements are distinct small integers, so a shifted,
/// duplicated or truncated copy cannot pass the structural comparison.
fn tuple_elems(words: usize) -> Vec<Eterm> {
    (0..words.saturating_sub(1))
        .map(|i| make_small(i as i64))
        .collect()
}

/// The small "host-local" tuple a host would hold by raw `Eterm` into the receiver's own nursery:
/// three distinct small integers (so a recycled word is detectable), occupying 4 words.
fn local_tuple_elems() -> Vec<Eterm> {
    vec![make_small(7), make_small(8), make_small(9)]
}

/// While **relocation-exempt**, a delivery too large for the nursery is refused with the **raw**
/// allocation failure and the heap is left completely untouched: no collection ran, the nursery
/// base did not move, and the Rust-local term the host holds stays structural.
#[test]
fn exempt_process_surfaces_the_raw_refusal_without_relocating() -> Result<()> {
    let mut receiver = Process::new(pid(1))?;
    let mut src = source();

    // The receiver is NOT exempt by default — the exemption is opt-in per process.
    assert!(
        !receiver.is_relocation_exempt(),
        "the exemption must default to false (today's behaviour)"
    );

    // A term the host holds by raw `Eterm` into the receiver's nursery — deliberately NOT rooted
    // (empty registers, empty mailbox, empty dict, no saved args, no pending signals), exactly the
    // "unpublished live root" the exemption exists for.
    let local = receiver
        .heap_mut()
        .tuple(&local_tuple_elems())
        .expect("the local term fits the nursery");
    let local_words = size_object(local);
    assert_eq!(local_words, 4, "header + three elements");

    // Snapshot the relocation-sensitive state BEFORE the exempt delivery.
    let minor_before = receiver.collector().minor_gcs();
    let base_before = receiver.collector().young().base();
    let used_before = receiver.heap_used_words();
    assert_eq!(used_before, local_words, "only the local term is live");

    receiver.set_relocation_exempt(true);
    assert!(receiver.is_relocation_exempt());

    // One word larger than the whole nursery: refused by the copier's preflight (which writes
    // nothing) — the exempt path must surface this refusal verbatim rather than collect/grow.
    let capacity = receiver.collector().young().capacity_words();
    let payload_words = capacity + 1;
    let elems = tuple_elems(payload_words);
    let payload = src
        .tuple(&elems)
        .expect("the payload tuple fits the source heap");
    assert_eq!(size_object(payload), payload_words, "header + elements");

    let from = pid(2).to_eterm();
    match receiver.deliver(from, payload, &OffHeapTable::new()) {
        Err(BeamError::Alloc(failure)) => assert!(
            matches!(failure.kind, AllocKind::Heap | AllocKind::Message),
            "the raw refusal keeps its Heap/Message kind, got {:?}",
            failure.kind
        ),
        other => panic!("expected the raw allocation refusal, got {other:?}"),
    }

    // (b) NO relocation happened: the collector never ran and the nursery base never moved. Both
    // assertions FAIL the instant `deliver` takes the collect-and-grow path despite the exemption.
    assert_eq!(
        receiver.collector().minor_gcs(),
        minor_before,
        "a relocation-exempt delivery must not collect"
    );
    assert_eq!(
        receiver.collector().young().base(),
        base_before,
        "a relocation-exempt delivery must not relocate (or grow) the nursery"
    );
    // Nothing was written and nothing was enqueued: the refusal is clean.
    assert_eq!(
        receiver.heap_used_words(),
        used_before,
        "a refused payload writes nothing (capacity is preflighted)"
    );
    assert_eq!(receiver.mailbox().len(), 0, "a refused payload is not enqueued");

    // (c) The Rust-local term is still structurally intact — the words under the host's handle did
    // not move or get recycled.
    assert_eq!(
        size_object(local),
        local_words,
        "the local term's header/elements are unchanged"
    );
    let expected_local = src
        .tuple(&local_tuple_elems())
        .expect("the expected local tuple fits the source heap");
    assert!(
        compare::eq(local, expected_local, &AtomTable::new()),
        "the local term is structurally identical after the refused delivery"
    );

    Ok(())
}

/// The **counterpart**: the exact same delivery on an identical process WITHOUT the exemption
/// takes the collect-and-grow path — one collection runs, the nursery is grown on demand, and the
/// oversized message lands in the mailbox whole. This pins that the exemption, not the refusal, is
/// what changes the outcome above.
#[test]
fn non_exempt_process_collects_and_grows_for_the_same_delivery() -> Result<()> {
    let mut receiver = Process::new(pid(3))?;
    let mut src = source();
    assert!(!receiver.is_relocation_exempt());

    // Identical setup: the same host-local term sits un-rooted in the nursery.
    let _local = receiver
        .heap_mut()
        .tuple(&local_tuple_elems())
        .expect("the local term fits the nursery");
    let minor_before = receiver.collector().minor_gcs();
    let base_before = receiver.collector().young().base();

    let capacity = receiver.collector().young().capacity_words();
    let payload_words = capacity + 1;
    let elems = tuple_elems(payload_words);
    let payload = src
        .tuple(&elems)
        .expect("the payload tuple fits the source heap");

    let from = pid(4).to_eterm();
    let outcome = receiver.deliver(from, payload, &OffHeapTable::new())?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));

    // The refused copy collected exactly once (the on-demand growth's single collection) ...
    assert_eq!(
        receiver.collector().minor_gcs(),
        minor_before + 1,
        "the unexempt retry collected exactly once"
    );
    // ... and grew the nursery on demand, which relocates it (the base moves).
    assert_eq!(
        receiver.collector().young().capacity_words(),
        next_heap_size(payload_words, 0),
        "the retry grew the nursery to the smallest curated size covering the request"
    );
    assert_ne!(
        receiver.collector().young().base(),
        base_before,
        "the on-demand growth replaces the nursery (a relocation)"
    );

    // The payload was enqueued whole and exactly once.
    assert_eq!(receiver.mailbox().len(), 1, "exactly one message");
    let copied = receiver.mailbox().peek().expect("one message").payload();
    assert_eq!(size_object(copied), payload_words, "copied whole");
    let expected = src
        .tuple(&elems)
        .expect("the expected tuple fits the source heap");
    assert!(
        compare::eq(copied, expected, &AtomTable::new()),
        "the copied payload equals the sent one element for element"
    );

    Ok(())
}

/// The defensive entries: while exempt, `collect_if_needed` is a no-op answering `Ok(false)`, and
/// `collect` / `ensure_capacity` refuse with the typed `relocation_exempt` raise — `collector`
/// untouched in every case.
#[test]
fn exempt_process_refuses_the_defensive_collection_entries() -> Result<()> {
    let mut process = Process::new(pid(5))?;
    process.set_relocation_exempt(true);
    assert!(process.is_relocation_exempt());

    // `collect_if_needed` is a no-op even when a collection WOULD be needed: its return value means
    // "a collection ran", and none did.
    assert!(
        !process.collect_if_needed(usize::MAX)?,
        "an exempt process never collects here, even for an impossible request"
    );

    // `collect` refuses with the typed raise rather than relocating.
    match process.collect() {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some("relocation_exempt: collect"),
            "collect's refusal carries the typed relocation_exempt reason"
        ),
        other => panic!("expected the typed relocation_exempt raise, got {other:?}"),
    }

    // `ensure_capacity` refuses with the typed raise rather than collecting+growing.
    match process.ensure_capacity(1024) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some("relocation_exempt: ensure_capacity"),
            "ensure_capacity's refusal carries the typed relocation_exempt reason"
        ),
        other => panic!("expected the typed relocation_exempt raise, got {other:?}"),
    }

    // No collection ran in any of the three entries.
    assert_eq!(
        process.collector().minor_gcs(),
        0,
        "none of the exempt defensive entries collected"
    );

    // The exemption is reversible, and an unexempt process collects normally again.
    process.set_relocation_exempt(false);
    assert!(!process.is_relocation_exempt());
    process.collect()?;
    assert_eq!(
        process.collector().minor_gcs(),
        1,
        "clearing the exemption restores the ordinary collect path"
    );

    Ok(())
}

/// The `ProcessTable` convenience delegates to the PCB, with this file's error style: an unknown
/// pid is `TermNotFound`.
#[test]
fn ptab_exemption_convenience_round_trips() -> Result<()> {
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
    let pid = table.spawn_default()?;

    assert!(
        !table.is_relocation_exempt(pid)?,
        "a fresh process is not exempt"
    );
    table.set_relocation_exempt(pid, true)?;
    assert!(table.is_relocation_exempt(pid)?);
    // The mark really reached the PCB.
    assert!(table.get(pid).expect("live process").is_relocation_exempt());
    table.set_relocation_exempt(pid, false)?;
    assert!(!table.is_relocation_exempt(pid)?);

    // An unknown pid is the table's existing `TermNotFound`.
    let unknown = ProcessId::new(9999, 0);
    assert!(!table.contains(unknown), "the pid must be unknown");
    assert!(matches!(
        table.set_relocation_exempt(unknown, true),
        Err(BeamError::TermNotFound)
    ));
    assert!(matches!(
        table.is_relocation_exempt(unknown),
        Err(BeamError::TermNotFound)
    ));

    Ok(())
}
