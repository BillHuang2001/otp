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

//! **The process's reusable collection scratch is a pure allocation saving** — two consecutive
//! collections over one [`Process`], each with its own root layout, report exactly what a freshly
//! constructed process reports, and no off-heap payload is retained or freed by a *previous*
//! round's verdict.
//!
//! A collection runs at every heap admission, so the three buffers it needs — the flat root slice,
//! the list of messages drained out of the mailbox while their envelope words are roots, and the
//! set of off-heap payload pointers a reachability walk admitted — are owned once by the process
//! (`beam_process`'s `scratch::CollectionScratch`) instead of being re-allocated per round. Reuse is
//! only sound because every entry point clears all three **before** it admits anything, and each
//! stale buffer is a real hazard, not bookkeeping:
//!
//! * a stale **root word** is a **wrong root set** — a word the collection treats as live, so it
//!   relocates a term out of a from-space a previous collection already evacuated or freed, writes
//!   the result into a root region, and counts it in the off-heap reachability sweep;
//! * a stale **message** in the drained list is a **duplicated / dangling message** — it owns a
//!   fragment chain and the off-heap refcounts of what it copied, so re-installing it would
//!   duplicate a mailbox entry and double-drop those refcounts;
//! * a stale **payload pointer** in the reachability set retains an off-heap binary nothing names
//!   any more — a leak of a whole payload (and, mirrored, a set that is cleared *after* its walk
//!   would free a payload that is still live: a dangle).
//!
//! The properties pinned here are therefore watched, never assumed, and every one of them is forced
//! by an explicit API call — no sleep, no wall-clock bound, no oracle:
//!
//! 1. **A different second-round root layout reports only its own live set.** Round one queues
//!    several messages, fills the dictionary, saves a BIF argument, publishes an extra root and
//!    queues a signal; round two drains/empties all of it and builds a *different* layout. Round
//!    two's figures are the second layout's exactly (`words_copied`/`objects_copied` equal to a
//!    fresh process's), the mailbox holds exactly round two's messages in FIFO order with nothing
//!    resurrected, and every region reads back intact.
//! 2. **The reachability set does not carry a verdict across rounds**: a payload reachable in round
//!    one and unreachable in round two is released, and the mirrored direction (unreachable, then
//!    reachable) retains.
//! 3. **A failed collection leaves no residue**: after a typed `Err` (whose write-back ran and whose
//!    root buffer stayed populated), the next successful collection reports exactly what a fresh
//!    process reports.

use beam_alloc::{AllocatorConfig, AllocRef, CarrierAllocator, Heap, RefcBinary};
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::prelude::CollectionKind;
use beam_process::prelude::*;
use beam_term::prelude::{
    is_small, make_small, ptr_val, signed_value, tuple_element, tuple_size, write_refc_binary,
    Eterm, ERL_REFC_BITS_SIZE, NIL,
};
// ---------------------------------------------------------------------------
// shared fixtures
// ---------------------------------------------------------------------------

/// A fresh process with pid `number` and the default heap policy.
fn new_process(number: u64) -> Process {
    Process::new(ProcessId::new(number, 0)).expect("the default heap allocates")
}

/// A 2-element tuple `{tag, tag}` built on `heap` — a **boxed** term, so a collection really has to
/// relocate it and `words_copied` counts it (3 words).
///
/// `tag` is an immediate, so the tuple is read back with no reference to the heap it started on:
/// its elements are the same small integers after any number of relocations.
fn tagged_tuple(heap: &mut Heap, tag: i64) -> Eterm {
    heap.tuple(&[make_small(tag), make_small(tag)])
        .expect("the 3-word tuple fits the young heap")
}

/// The `{tag, tag}` tuple's tag, read back through the term itself.
///
/// # Safety
///
/// `term` must be a live 2-element tuple, i.e. a term [`tagged_tuple`] built and then kept rooted
/// across every collection since.
unsafe fn tuple_tag(term: Eterm) -> i64 {
    // SAFETY: the caller guarantees `term` is the live 2-element tuple `{tag, tag}`, so slots 0..2
    // are in bounds.
    unsafe {
        assert_eq!(tuple_size(term), 2, "the fixture tuple has two elements");
        let first = tuple_element(term, 0);
        assert_eq!(first, tuple_element(term, 1), "the elements agree");
        assert!(is_small(first), "the fixture tuple's element is a small int");
        signed_value(first)
    }
}
/// Every queued message's payload tag, in FIFO order.
fn mailbox_tags(process: &Process) -> Vec<i64> {
    process
        .mailbox()
        .iter()
        .map(|message| {
            // SAFETY: every message this file enqueues carries a `tagged_tuple` payload, kept
            // rooted by the mailbox across every collection.
            unsafe { tuple_tag(message.payload()) }
        })
        .collect()
}

/// An isolated carrier allocator, so a payload block's allocation/free is observable through
/// `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(name: &str) -> AllocRef {
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

/// A 200-byte off-heap payload (`> ERL_ONHEAP_BINARY_LIMIT`, so its bytes really live in an
/// off-heap counted object) whose bytes depend on `seed`, so a stale or wrong payload is visible.
fn off_heap_payload(alloc: &AllocRef, seed: u8) -> Result<RefcBinary> {
    let bytes: Vec<u8> = (0..200usize)
        .map(|i| ((i * 7 + seed as usize) % 251) as u8)
        .collect();
    RefcBinary::with_allocator(alloc.clone(), &bytes)
}

/// Writes the on-heap `[BinRef][ErlSubBits]` pair naming `binary`'s payload on the process's own
/// young heap, and returns the tagged root (the `ErlSubBits`).
///
/// # Safety
///
/// The process (or the caller's own handle) must keep `binary`'s payload alive for as long as the
/// returned term is reachable, and the young heap must have [`ERL_REFC_BITS_SIZE`] spare words.
unsafe fn bin_ref_pair(process: &mut Process, binary: &RefcBinary) -> Eterm {
    // SAFETY: the caller keeps the payload alive for the returned term's lifetime and the payload
    // pointer is word-aligned (`beam_alloc` hands out `base + HEADER_SIZE`, a whole unit).
    unsafe {
        let hp = process
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("the 8-word refc pair fits the young heap");
        write_refc_binary(
            hp,
            binary.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            binary.data_ptr(),
            0,
            binary.as_bytes().len() * 8,
        )
    }
}

/// Roots `terms` in `x(0..terms.len())`.
fn root_in_registers(process: &mut Process, terms: &[Eterm]) -> Result<()> {
    process.registers_mut().x_mut().set_live(terms.len())?;
    for (index, &term) in terms.iter().enumerate() {
        process.registers_mut().x_mut().set(index, term)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// (1) a different second-round root layout reports only its own live set
// ---------------------------------------------------------------------------

/// Round one's three queued messages and round two's single one — distinct tags, so a payload from
/// the wrong round is unmistakable.
const ROUND_ONE_TAGS: [i64; 3] = [11, 12, 13];
const ROUND_TWO_TAG: i64 = 21;

/// The collection figures a **fresh** process reports for the round-two layout: one 3-word payload
/// tuple plus one 3-word extra-root tuple — `(words_copied, objects_copied)`. A reused root buffer
/// that failed to clear would add round one's words (and, since they point into an evacuated
/// space, would be a wrong root set rather than merely a bigger count).
const ROUND_TWO_FIGURES: (usize, usize) = (6, 2);

/// The second round's live layout, built identically on any process: one queued message and one
/// host-published extra root (the dictionary and the saved arguments are emptied first).
fn build_round_two(process: &mut Process, source: &mut Process) -> Result<()> {
    while process.receive(|_| true).is_some() {}
    process.dict_mut().erase_all();
    process.saved_args_mut().clear();
    process.registers_mut().extra_roots_mut().clear();

    let payload = tagged_tuple(source.heap_mut(), ROUND_TWO_TAG);
    process.deliver_simple(make_small(101), payload)?;
    let extra = tagged_tuple(process.heap_mut(), 71);
    process.registers_mut().extra_roots_mut().push(extra);
    Ok(())
}

#[test]
fn a_second_collection_with_a_different_root_layout_reports_only_its_own() -> Result<()> {
    let mut source = new_process(900);
    let mut process = new_process(1);

    // --- round one: the fullest layout this file ever builds -------------------------------
    for (index, tag) in ROUND_ONE_TAGS.iter().enumerate() {
        let payload = tagged_tuple(source.heap_mut(), *tag);
        process.deliver_simple(make_small(index as i64 + 1), payload)?;
    }
    let dict_one = tagged_tuple(process.heap_mut(), 31);
    let dict_two = tagged_tuple(process.heap_mut(), 32);
    process.dict_mut().put(make_small(1), dict_one);
    process.dict_mut().put(make_small(2), dict_two);
    let saved = process
        .heap_mut()
        .cons(make_small(41), NIL)
        .expect("the saved-argument cons fits");
    process.saved_args_mut().push(saved);
    let extra_one = tagged_tuple(process.heap_mut(), 51);
    process.registers_mut().extra_roots_mut().push(extra_one);
    // A queued exit signal whose reason lives in this process's own heap, so the pending-signal
    // region is a real root across both collections.
    let reason = tagged_tuple(process.heap_mut(), 61);
    process.signals_mut().push(PendingSignal::exit(
        ProcessId::new(901, 0),
        reason,
        ExitFamily::Explicit,
    ));

    assert_eq!(mailbox_tags(&process), ROUND_ONE_TAGS.to_vec());
    assert_eq!(process.dict().len(), 2);
    assert_eq!(process.saved_args().len(), 1);
    assert_eq!(process.registers().extra_roots().len(), 1);
    assert_eq!(process.signals().len(), 1);

    let first = process.collect_kind(CollectionKind::Minor)?;
    assert_eq!(first.kind, CollectionKind::Minor);
    assert_eq!(process.collector().stats().minor_collections, 1);
    assert_eq!(process.collector().stats().major_collections, 0);
    assert_eq!(process.collector().minor_gcs(), 1);

    // Round one's own layout survived it: the mailbox is intact and in FIFO order, both dict
    // values read back with both of their elements (the test above the round-two build re-reads
    // them), and the extra root and the signal reason are the terms they were.
    assert_eq!(mailbox_tags(&process), ROUND_ONE_TAGS.to_vec());
    assert_eq!(process.dict().len(), 2);
    assert_eq!(process.saved_args().len(), 1);
    assert_eq!(process.registers().extra_roots().len(), 1);
    assert_eq!(process.signals().len(), 1);

    // --- round two: every one of those regions is emptied and rebuilt differently -----------
    build_round_two(&mut process, &mut source)?;
    assert_eq!(mailbox_tags(&process), vec![ROUND_TWO_TAG]);
    assert_eq!(process.dict().len(), 0);
    assert_eq!(process.saved_args().len(), 0);
    assert_eq!(process.registers().extra_roots().len(), 1);
    // The round-one pending signal is deliberately **kept**: it must survive a second collection
    // that shares none of the rest of the layout.
    assert_eq!(process.signals().len(), 1);

    let second = process.collect_kind(CollectionKind::Minor)?;
    assert_eq!(second.kind, CollectionKind::Minor);

    // 1a. No message is resurrected, duplicated or reordered: exactly round two's single message,
    // and nothing at all left over from round one.
    assert_eq!(mailbox_tags(&process), vec![ROUND_TWO_TAG]);
    assert_eq!(process.mailbox().len(), 1);
    // ... and the same is true of a **fresh** process that never had round one's layout at all.
    let mut fresh = new_process(2);
    let mut fresh_source = new_process(902);
    build_round_two(&mut fresh, &mut fresh_source)?;
    let reference = fresh.collect_kind(CollectionKind::Minor)?;
    assert_eq!(mailbox_tags(&fresh), mailbox_tags(&process));
    assert_eq!(fresh.mailbox().len(), process.mailbox().len());

    // 1b. Round two's collection copied round two's live set and nothing else — the identical
    // figure for a process whose scratch has only ever held round two's roots.
    assert_eq!(
        (second.stats.words_copied, second.stats.objects_copied),
        ROUND_TWO_FIGURES,
        "only round two's own live set is copied"
    );
    assert_eq!(
        (second.stats.words_copied, second.stats.objects_copied),
        (reference.stats.words_copied, reference.stats.objects_copied),
        "the reused scratch must report what a fresh one does"
    );

    // 1c. The counters account for exactly the collections that ran.
    assert_eq!(process.collector().stats().minor_collections, 2);
    assert_eq!(process.collector().stats().major_collections, 0);
    assert_eq!(process.collector().minor_gcs(), 2);
    assert_eq!(
        fresh.collector().stats().minor_collections + fresh.collector().stats().major_collections,
        1,
        "the reference process ran exactly one collection"
    );

    // 1d. Every surviving region reads back intact, and its terms live in a live space.
    let extra_after = *process
        .registers()
        .extra_roots()
        .first()
        .expect("the round-two extra root survived");
    // SAFETY: the extra root is the live `{71, 71}` tuple pushed by `build_round_two` and rooted
    // across the collection, so its two elements are in bounds.
    assert_eq!(unsafe { tuple_tag(extra_after) }, 71);
    assert!(
        process.collector().young_contains(ptr_val(extra_after))
            || process.collector().mature_contains(ptr_val(extra_after)),
        "the extra root was written back into a live space"
    );

    let message = process
        .mailbox()
        .iter()
        .next()
        .expect("the round-two message survived")
        .payload();
    // SAFETY: the payload is the live `{21, 21}` tuple delivered by `build_round_two` and rooted by
    // the mailbox across the collection.
    assert_eq!(unsafe { tuple_tag(message) }, ROUND_TWO_TAG);
    assert!(
        process.collector().young_contains(ptr_val(message))
            || process.collector().mature_contains(ptr_val(message)),
        "the message payload was written back into a live space"
    );

    // The round-one pending signal and its heap-resident reason survived both collections: the
    // reason word was relocated by round one and is untouched by round two.
    let signal_reason = process
        .signals()
        .iter()
        .next()
        .expect("the round-one signal survived both collections")
        .reason();
    // SAFETY: the reason is the live `{61, 61}` tuple built before round one and rooted by the
    // pending-signal queue across both collections.
    assert_eq!(unsafe { tuple_tag(signal_reason) }, 61);
    assert!(
        process.collector().young_contains(ptr_val(signal_reason))
            || process.collector().mature_contains(ptr_val(signal_reason)),
        "the signal reason was written back into a live space"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// (2) the off-heap reachability set carries no verdict across rounds
// ---------------------------------------------------------------------------

/// A payload named by a live on-heap `[BinRef][ErlSubBits]` handle in round one and by **nothing**
/// in round two is released in round two — a stale `reachable` entry from round one cannot keep it
/// alive.
#[test]
fn a_payload_reachable_in_the_first_round_is_released_when_the_second_round_drops_it() -> Result<()> {
    let alloc = test_allocator("scratch-reuse-reachable-then-not");
    let binary = off_heap_payload(&alloc, 1)?;
    let data_ptr = binary.data_ptr();

    let mut process = new_process(3);
    let slot = process.register_off_heap_binary(binary.clone());
    // SAFETY: the process's own handle keeps the payload alive, and the 8-word pair fits.
    let root = unsafe { bin_ref_pair(&mut process, &binary) };
    root_in_registers(&mut process, &[root])?;

    // Round one: reachable ⇒ retained.
    process.collect_kind(CollectionKind::Minor)?;
    let held = process
        .off_heap_binary(slot)
        .expect("the reachable payload survived round one");
    assert_eq!(held.data_ptr(), data_ptr, "the payload never moves");
    assert_eq!(held.as_bytes(), binary.as_bytes());
    assert_eq!(held.refcount(), 2, "the process's handle plus this test's");
    // Drop this test's handle: the process (which owns the registered payload) is now the only
    // owner, so a release is observable as the payload block being freed.
    drop(binary);
    assert_eq!(alloc.stats().live_blocks, 1, "only the process holds it");

    // Round two: the handle is no longer a root (only the dead pair in the mature space still names
    // it), so the walk admits nothing and the payload is released.
    process.registers_mut().x_mut().set_live(0)?;
    process.collect_kind(CollectionKind::Minor)?;
    assert!(
        process.off_heap_binary(slot).is_none(),
        "a stale `reachable` entry from round one would have retained the payload"
    );
    assert_eq!(process.off_heap_binary_count(), 0);
    assert_eq!(
        alloc.stats().live_blocks,
        0,
        "the payload block was freed at reference count 0"
    );
    assert_eq!(alloc.stats().live_bytes, 0);
    Ok(())
}

/// The mirror: a payload that is **unreachable** in round one (so the reachability set ends round
/// one empty) and reachable in round two is retained — the set is cleared *before* its walk admits
/// anything, never after.
#[test]
fn a_payload_unreachable_in_the_first_round_is_retained_once_the_second_round_roots_it() -> Result<()> {
    let alloc = test_allocator("scratch-reuse-unreachable-then");
    let first = off_heap_payload(&alloc, 2)?;
    let second = off_heap_payload(&alloc, 3)?;

    let mut process = new_process(4);
    // Round one: registered but named by no on-heap handle ⇒ released.
    let first_slot = process.register_off_heap_binary(first);
    process.collect_kind(CollectionKind::Minor)?;
    assert!(
        process.off_heap_binary(first_slot).is_none(),
        "an unreachable payload is released in round one"
    );
    assert_eq!(alloc.stats().live_blocks, 1, "only the second payload is left");

    // Round two: a *reachable* payload, so the walk admits it — a set cleared after the walk (or
    // not at all, with an empty verdict reused) would release a live payload, a dangle.
    let second_slot = process.register_off_heap_binary(second.clone());
    // SAFETY: the process's own handle keeps the payload alive, and the 8-word pair fits.
    let root = unsafe { bin_ref_pair(&mut process, &second) };
    root_in_registers(&mut process, &[root])?;
    process.collect_kind(CollectionKind::Minor)?;

    let held = process
        .off_heap_binary(second_slot)
        .expect("the round-two reachable payload was retained");
    assert_eq!(held.data_ptr(), second.data_ptr());
    assert_eq!(held.as_bytes(), second.as_bytes());
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(alloc.stats().live_blocks, 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// (3) a failed collection leaves no residue for the next one
// ---------------------------------------------------------------------------

/// A collection action that fails still runs its write-back, so the root buffer stays populated.
/// The **next** collection must report exactly what a fresh process reports — proving the buffer was
/// cleared rather than appended to.
///
/// The failure is the typed heap exhaustion of a pinned minor whose promotion cannot fit:
/// [`Process::collect_kind`] refuses to escalate a `Minor` to a `Major`, so a 20-cons-cell live set
/// (40 words) against an 8-word mature space fails **before a single word is copied**, leaving the
/// nursery, the mature space and the counters untouched.
#[test]
fn a_failed_collection_leaves_no_residue_for_the_next_one() -> Result<()> {
    let policy = beam_gc::prelude::HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 8,
        ..beam_gc::prelude::HeapPolicy::default()
    };
    let mut process = Process::with_policy(ProcessId::new(5, 0), policy.clone())?;

    // 20 cons cells = 40 live words, every one rooted in the extra-root slot.
    for tag in 0..20i64 {
        let cell = process
            .heap_mut()
            .cons(make_small(tag), NIL)
            .expect("the cons cell fits the nursery");
        process.registers_mut().extra_roots_mut().push(cell);
    }
    let stats_before = process.collector().stats();
    match process.collect_kind(CollectionKind::Minor) {
        Err(BeamError::Alloc(failure)) => assert_eq!(
            failure.kind,
            AllocKind::Heap,
            "a minor that cannot fit is the heap exhaustion"
        ),
        other => panic!("expected the typed heap exhaustion, got {other:?}"),
    }
    assert_eq!(
        process.collector().stats(),
        stats_before,
        "the failed collection mutated nothing"
    );

    // Those 40 words are garbage now: drop them from the root list and root a single 3-word tuple,
    // so the next collection's live set fits the 8-word mature space.
    process.registers_mut().extra_roots_mut().clear();
    let live = tagged_tuple(process.heap_mut(), 7);
    process.registers_mut().extra_roots_mut().push(live);

    let collection = process.collect_kind(CollectionKind::Minor)?;
    assert_eq!(collection.kind, CollectionKind::Minor);
    assert_eq!(
        (collection.stats.words_copied, collection.stats.objects_copied),
        (3, 1),
        "only the new live term is copied — a root word left by the failed call would show here"
    );
    assert_eq!(
        process.collector().stats().minor_collections, 1,
        "exactly one collection ever ran on this process"
    );

    // The reference: a **fresh** process with the same policy and the same live layout reports the
    // same figures, so the failed call's leftovers were invisible.
    let mut reference = Process::with_policy(ProcessId::new(6, 0), policy)?;
    let reference_live = tagged_tuple(reference.heap_mut(), 7);
    reference.registers_mut().extra_roots_mut().push(reference_live);
    let expected = reference.collect_kind(CollectionKind::Minor)?;
    assert_eq!(
        (collection.stats.words_copied, collection.stats.objects_copied),
        (expected.stats.words_copied, expected.stats.objects_copied),
        "the post-failure collection must report what a fresh one does"
    );

    // The surviving term is the live `{7, 7}` tuple, in a live space.
    let after = *process
        .registers()
        .extra_roots()
        .first()
        .expect("the live extra root survived");
    // SAFETY: the extra root is the live `{7, 7}` tuple rooted across the collection.
    assert_eq!(unsafe { tuple_tag(after) }, 7);
    assert!(
        process.collector().young_contains(ptr_val(after))
            || process.collector().mature_contains(ptr_val(after)),
        "the live term was written back into a live space"
    );
    Ok(())
}
