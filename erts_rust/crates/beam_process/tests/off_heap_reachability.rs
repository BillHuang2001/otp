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

//! Integration evidence for the **reachability-driven** off-heap refc-binary liveness decision.
//!
//! [`Process::register_off_heap_binary`] makes the process the **owner** of an off-heap payload, but
//! the on-heap `[BinRef(3)][ErlSubBits(5)]` pair that names it is nested inside boxed terms, so the
//! process could not decide liveness on its own: the original reclamation step released only what the
//! caller had **declared dead** and retained everything undecided (leak-not-dangle, pinned by
//! `off_heap_binaries.rs`).
//!
//! `beam_gc`'s public, read-only, whole-heap reachability walk
//! ([`beam_gc::AnyCollector::for_each_reachable_object`]) closes that gap, and this file pins the
//! `beam_process` half that consumes it:
//!
//! * [`Process::sweep_off_heap_binaries_reachable`] releases exactly the owned payloads **no
//!   reachable on-heap `BinRef` names** any more — so a payload still referenced by a handle (a bare
//!   pair, or one nested inside a tuple/list/map, or reached through any of the process's in-heap
//!   root regions) is **kept**, and one nothing reaches is **reclaimed**;
//! * [`Process::run_rooted_collection`] runs it automatically over the roots the collection just
//!   gathered, with the conservative declared-dead sweep as the fallback for a process whose root set
//!   is incomplete ([`Process::has_unpublished_roots`]: relocation-exempt or `Running`), where a
//!   reachability verdict could miss a live handle and release a live payload — a **dangle**.
//!
//! Determinism: every verdict is forced by an explicit API call — a fixed 200-byte payload (>
//! `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes, so its bytes really live in an off-heap counted object), an
//! **injected** `AllocRef` so a payload block's allocation/free is observable in isolation, and
//! explicit `sweep`/`collect` calls. No sleep, no wall-clock bound, no oracle.

use beam_alloc::{AllocatorConfig, AllocRef, CarrierAllocator, Heap, RefcBinary};
use beam_common::Result;
use beam_message::Message;
use beam_process::prelude::*;
use beam_term::prelude::{
    boxed_val, is_refc_binary, make_small, map_words_capacity, refc_size, write_map,
    write_refc_binary, AtomTable, Eterm, ERL_ONHEAP_BINARY_LIMIT, ERL_REFC_BITS_SIZE, NIL,
};

/// A 200-byte payload whose bytes are position-dependent, so a shifted, duplicated or truncated
/// view of the payload is visible.
fn payload() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// An isolated carrier allocator, so a test can observe a binary's payload block being freed through
/// `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(name: &str) -> AllocRef {
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

/// Allocates an off-heap payload of [`payload`] on `alloc`.
fn off_heap_payload(alloc: &AllocRef) -> Result<RefcBinary> {
    RefcBinary::with_allocator(alloc.clone(), &payload())
}

/// Writes the on-heap `[BinRef][ErlSubBits]` pair naming `data_ptr`/`len` on `heap` and returns the
/// tagged root (the `ErlSubBits`).
///
/// # Safety
///
/// `heap` must have [`ERL_REFC_BITS_SIZE`] spare words and `data_ptr` must be a live, word-aligned
/// payload of `len` bytes that outlives the returned term.
unsafe fn refc_binary_term(heap: &mut Heap, data_ptr: *mut u8, len: usize) -> Eterm {
    // SAFETY: the caller keeps the payload alive for the returned term's lifetime, and `data_ptr`
    // is word-aligned (`beam_alloc` hands out `base + HEADER_SIZE` with `HEADER_SIZE` a unit), so
    // `write_refc_binary`'s flag-bit assert holds.
    unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            len * 8,
        )
    }
}

/// An on-heap `[BinRef][ErlSubBits]` pair for `binary`'s payload, built on the process's own heap.
fn bin_ref_pair(process: &mut Process, binary: &RefcBinary) -> Eterm {
    // SAFETY: the process (or the caller's own handle) keeps the payload alive for the term's
    // lifetime, and the 8-word pair fits the young heap.
    unsafe { refc_binary_term(process.heap_mut(), binary.data_ptr(), payload().len()) }
}

/// A fresh process with a normal heap and an empty root set.
fn process(pid: u64) -> Process {
    Process::new(ProcessId::new(pid, 0)).expect("the default heap allocates")
}

/// Roots `terms` in `x(0..terms.len())`, so the reachability walk sees them as process roots.
fn root_in_registers(process: &mut Process, terms: &[Eterm]) -> Result<()> {
    process.registers_mut().x_mut().set_live(terms.len())?;
    for (index, &term) in terms.iter().enumerate() {
        process.registers_mut().x_mut().set(index, term)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// (a) reachable ⇒ retained    (b) unreachable ⇒ reclaimed
// ---------------------------------------------------------------------------

/// **(a)** A payload named by a reachable on-heap `BinRef` pair is **retained** by the reachability
/// sweep — nothing is released, the slot still resolves, and the payload's bytes and reference count
/// are untouched.
#[test]
fn a_payload_reachable_through_an_on_heap_handle_is_retained() -> Result<()> {
    let bytes = payload();
    assert!(
        bytes.len() > ERL_ONHEAP_BINARY_LIMIT,
        "the payload must exceed the on-heap limit so it is an off-heap refc binary"
    );
    let alloc = test_allocator("offheap-reach-retained");
    let binary = off_heap_payload(&alloc)?;
    let data_ptr = binary.data_ptr();

    let mut process = process(1);
    let slot = process.register_off_heap_binary(binary.clone());
    let root = bin_ref_pair(&mut process, &binary);
    root_in_registers(&mut process, &[root])?;

    assert_eq!(
        process.sweep_off_heap_binaries_reachable()?,
        0,
        "the payload is named by a live on-heap handle, so nothing is released"
    );
    let held = process.off_heap_binary(slot).expect("the entry survived");
    assert_eq!(held.data_ptr(), data_ptr, "the payload pointer is unchanged");
    assert_eq!(held.as_bytes(), &bytes[..], "the payload bytes are unchanged");
    assert_eq!(held.refcount(), 2, "the process's handle plus the local owner");
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(alloc.stats().live_blocks, 1, "the payload block is still live");
    Ok(())
}

/// **(b)** A payload no reachable handle names is **reclaimed** — the payload is freed at reference
/// count 0 (observed through the injected allocator), its slot is released, and a second sweep
/// releases nothing further.
#[test]
fn an_unreachable_payload_is_reclaimed() -> Result<()> {
    let alloc = test_allocator("offheap-reach-unreachable");
    let binary = off_heap_payload(&alloc)?;
    let live_bytes = alloc.stats().live_bytes;
    assert_eq!(alloc.stats().live_blocks, 1);

    let mut process = process(2);
    // The registration **moves** the sole handle into the process: nothing else can keep the
    // payload alive, and no on-heap `BinRef` names it.
    let slot = process.register_off_heap_binary(binary);
    assert_eq!(
        process.off_heap_binary(slot).expect("slot 0 resolves").refcount(),
        1,
        "the process holds the only handle"
    );

    assert_eq!(
        process.sweep_off_heap_binaries_reachable()?,
        1,
        "the unreachable payload was released"
    );
    assert!(process.off_heap_binary(slot).is_none(), "the slot is released");
    assert_eq!(process.off_heap_binary_count(), 0);
    assert_eq!(process.off_heap_binary_bytes(), 0);
    assert!(
        live_bytes > 0,
        "the payload really held bytes, so the free is observable"
    );
    assert_eq!(
        alloc.stats().live_blocks,
        0,
        "the payload block was freed at reference count 0"
    );
    assert_eq!(alloc.stats().live_bytes, 0);

    // No double free: a second sweep releases nothing (the released slot is skipped).
    assert_eq!(process.sweep_off_heap_binaries_reachable()?, 0);
    assert_eq!(alloc.stats().live_blocks, 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// (c) reachable only through a nested container ⇒ retained
// ---------------------------------------------------------------------------

/// **(c)** A payload reached only **through a term nested inside** a tuple, a list or a map is still
/// retained — the walk descends the container shapes — while a fourth payload, registered but rooted
/// nowhere, is reclaimed in the same sweep.
#[test]
fn a_payload_reached_only_through_a_nested_container_is_retained() -> Result<()> {
    let alloc = test_allocator("offheap-reach-nested");
    let atoms = AtomTable::new();

    let in_tuple = off_heap_payload(&alloc)?;
    let in_list = off_heap_payload(&alloc)?;
    let in_map = off_heap_payload(&alloc)?;
    let unreached = off_heap_payload(&alloc)?;

    let mut process = process(3);
    let tuple_slot = process.register_off_heap_binary(in_tuple.clone());
    let list_slot = process.register_off_heap_binary(in_list.clone());
    let map_slot = process.register_off_heap_binary(in_map.clone());
    let unreached_slot = process.register_off_heap_binary(unreached);
    assert_eq!(alloc.stats().live_blocks, 4);

    // `{BinRef…}` — the pair one level down from the tuple head.
    let tuple_term = bin_ref_pair(&mut process, &in_tuple);
    let tuple = process.heap_mut().tuple(&[make_small(7), tuple_term])?;
    // `[BinRef…]` — the pair as the cell's car.
    let list_term = bin_ref_pair(&mut process, &in_list);
    let list = process.heap_mut().cons(make_small(8), NIL)?;
    let list = process.heap_mut().cons(list_term, list)?;
    // `#{1 => BinRef…}` — the pair as a flat-map value.
    let map_term = bin_ref_pair(&mut process, &in_map);
    let hp = unsafe { process.heap_mut().alloc_words(map_words_capacity(1)) }?;
    // SAFETY: `hp` owns `map_words_capacity(1)` words on the process's young heap, and the key is an
    // immediate, so `write_map` needs no atom resolution (the empty table is the signature's
    // requirement only).
    let map = unsafe { write_map(hp, &[(make_small(1), map_term)], &atoms) }?;

    root_in_registers(&mut process, &[tuple, list, map])?;

    assert_eq!(
        process.sweep_off_heap_binaries_reachable()?,
        1,
        "only the payload rooted nowhere was released"
    );
    for (slot, binary) in [
        (tuple_slot, &in_tuple),
        (list_slot, &in_list),
        (map_slot, &in_map),
    ] {
        let held = process
            .off_heap_binary(slot)
            .unwrap_or_else(|| panic!("the nested payload at slot {slot} was retained"));
        assert_eq!(held.data_ptr(), binary.data_ptr());
        assert_eq!(held.as_bytes(), &payload()[..]);
    }
    assert!(
        process.off_heap_binary(unreached_slot).is_none(),
        "the payload no container reaches was reclaimed"
    );
    assert_eq!(process.off_heap_binary_count(), 3);
    assert_eq!(
        alloc.stats().live_blocks,
        3,
        "exactly the unrooted payload's block was freed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The walk covers the whole in-heap root set, not just the registers
// ---------------------------------------------------------------------------

/// A payload reached only through one of the process's **in-heap sub-rootsets** — a queued message's
/// payload, a process-dictionary value, a saved BIF/NIF argument, or a pending signal's reason — is
/// retained, so the scan-side gather cannot silently lag the collection's own gather.
#[test]
fn a_payload_reached_through_any_root_region_is_retained() -> Result<()> {
    let alloc = test_allocator("offheap-reach-regions");
    let in_mail = off_heap_payload(&alloc)?;
    let in_dict = off_heap_payload(&alloc)?;
    let in_saved = off_heap_payload(&alloc)?;
    let in_signal = off_heap_payload(&alloc)?;
    let unreached = off_heap_payload(&alloc)?;

    let mut process = process(4);
    let mail_slot = process.register_off_heap_binary(in_mail.clone());
    let dict_slot = process.register_off_heap_binary(in_dict.clone());
    let saved_slot = process.register_off_heap_binary(in_saved.clone());
    let signal_slot = process.register_off_heap_binary(in_signal.clone());
    let unreached_slot = process.register_off_heap_binary(unreached);
    assert_eq!(alloc.stats().live_blocks, 5);

    // A queued message whose payload is the refc pair (`from` stays an immediate, the envelope's
    // producer contract). No live register is set, so every region below is the term's only root.
    let mail_payload = bin_ref_pair(&mut process, &in_mail);
    process
        .mailbox_mut()
        .enqueue(Message::new(mail_payload, make_small(1)));

    let dict_value = bin_ref_pair(&mut process, &in_dict);
    process.dict_mut().put(make_small(2), dict_value);

    let saved_arg = bin_ref_pair(&mut process, &in_saved);
    process.saved_args_mut().push(saved_arg);

    let signal_reason = bin_ref_pair(&mut process, &in_signal);
    process.signals_mut().push(PendingSignal::exit(
        ProcessId::new(99, 0),
        signal_reason,
        ExitFamily::Explicit,
    ));

    assert_eq!(
        process.sweep_off_heap_binaries_reachable()?,
        1,
        "every region-rooted payload survived; only the unrooted one was released"
    );
    for (slot, binary) in [
        (mail_slot, &in_mail),
        (dict_slot, &in_dict),
        (saved_slot, &in_saved),
        (signal_slot, &in_signal),
    ] {
        assert_eq!(
            process
                .off_heap_binary(slot)
                .unwrap_or_else(|| panic!("the region-rooted payload at slot {slot} was retained"))
                .data_ptr(),
            binary.data_ptr(),
        );
    }
    assert!(process.off_heap_binary(unreached_slot).is_none());
    assert_eq!(process.off_heap_binary_count(), 4);
    assert_eq!(alloc.stats().live_blocks, 4);
    Ok(())
}

// ---------------------------------------------------------------------------
// (d) the decline valve: unpublished roots ⇒ never reachability-swept
// ---------------------------------------------------------------------------

/// **(d)** A process whose root set is **incomplete** is never reachability-swept: both cases of
/// [`Process::has_unpublished_roots`] — a **relocation-exempt** process and a currently **`Running`**
/// one — retain everything and report `0` releases, because the walk could miss a live handle and
/// release a payload a real reference still names.
#[test]
fn a_process_with_unpublished_roots_is_not_reachability_swept() -> Result<()> {
    let alloc = test_allocator("offheap-reach-declined");

    // (1) relocation-exempt: the host holds live terms outside the crate root set.
    let exempt_payload = off_heap_payload(&alloc)?;
    let mut exempt = process(5);
    let exempt_slot = exempt.register_off_heap_binary(exempt_payload.clone());
    exempt.set_relocation_exempt(true);
    assert!(exempt.has_unpublished_roots());
    assert_eq!(
        exempt.sweep_off_heap_binaries_reachable()?,
        0,
        "an exempt process is never reachability-swept, even with an unreachable payload"
    );
    assert_eq!(
        exempt.off_heap_binary_count(),
        1,
        "the payload was retained even though nothing on the heap names it"
    );
    assert_eq!(exempt.off_heap_binary(exempt_slot).expect("retained").refcount(), 2);

    // (2) Running: the process's own machine holds live terms the root set does not publish yet.
    let running_payload = off_heap_payload(&alloc)?;
    let mut running = process(6);
    running.register_off_heap_binary(running_payload.clone());
    running.set_state(ProcessState::Running);
    assert!(running.has_unpublished_roots());
    assert_eq!(
        running.sweep_off_heap_binaries_reachable()?,
        0,
        "a Running process is never reachability-swept"
    );
    assert_eq!(running.off_heap_binary_count(), 1, "the payload was retained");

    // The refusal is *only* about the reachability verdict: the conservative declared-dead path is
    // still available (and still releases exactly what the caller asserted).
    assert_eq!(running.sweep_off_heap_binaries(), 0, "nothing was declared dead");
    assert_eq!(running.off_heap_binary_count(), 1);

    assert_eq!(
        alloc.stats().live_blocks,
        2,
        "both declined processes kept their payloads' blocks alive"
    );
    Ok(())
}

/// A declined (`Running`) process that goes through a **real collection** still gets the conservative
/// declared-dead half of the reclamation step — and, symmetrically, its undeclared-but-unreachable
/// payload is **not** reclaimed, because the walk refused.
#[test]
fn a_declined_collection_still_honours_a_declared_death() -> Result<()> {
    let alloc = test_allocator("offheap-reach-declined-collection");

    // Declared dead: honoured by the collection's conservative half.
    let doomed = off_heap_payload(&alloc)?;
    // Never declared, never rooted: the reachability half would reclaim it, but it refuses here.
    let undecided = off_heap_payload(&alloc)?;

    let mut process = process(7);
    let doomed_slot = process.register_off_heap_binary(doomed);
    let undecided_slot = process.register_off_heap_binary(undecided.clone());
    assert!(process.declare_off_heap_binary_dead(doomed_slot));
    process.set_state(ProcessState::Running);

    // A `Running` process may still be collected (only a relocation-exempt one refuses); the roots
    // are the crate root set, which is empty here.
    process.collect()?;

    assert!(
        process.off_heap_binary(doomed_slot).is_none(),
        "the declared death was honoured by the conservative half"
    );
    let survivor = process
        .off_heap_binary(undecided_slot)
        .expect("the walk refused, so the undecided payload was retained");
    assert_eq!(survivor.data_ptr(), undecided.data_ptr());
    assert_eq!(survivor.refcount(), 2);
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(alloc.stats().live_blocks, 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// (e) the declared-dead / dedup / slot-stability contracts still hold
// ---------------------------------------------------------------------------

/// **(e)** The reachability sweep keeps the owned list's two structural contracts: **one reference
/// per payload** (a re-registration returns the existing slot and drops the surplus handle) and
/// **slot stability** (a survivor keeps its original slot, a released slot resolves to `None` and is
/// never reused).
#[test]
fn the_reachability_sweep_keeps_dedup_and_slot_stability() -> Result<()> {
    let alloc = test_allocator("offheap-reach-slots");
    let mut process = process(8);

    let mut ptrs = Vec::new();
    let mut slots = Vec::new();
    for _ in 0..3 {
        let binary = off_heap_payload(&alloc)?;
        ptrs.push(binary.data_ptr() as usize);
        // Each payload is **moved** into the process, so the list is its only owner and releasing the
        // entry really frees the block.
        slots.push(process.register_off_heap_binary(binary));
    }
    assert_eq!(slots, vec![0, 1, 2], "registrations append at fresh slots");
    assert_eq!(alloc.stats().live_blocks, 3);

    // Dedup: re-registering a payload the process already owns returns its existing slot and the
    // surplus handle is dropped, so the reference count is unchanged.
    let before = process.off_heap_binary(slots[1]).expect("live").refcount();
    assert_eq!(before, 1, "the process holds the sole handle");
    let duplicate = process.off_heap_binary(slots[1]).expect("live").clone();
    assert_eq!(process.register_off_heap_binary(duplicate), slots[1]);
    assert_eq!(
        process.off_heap_binary(slots[1]).expect("live").refcount(),
        before,
        "the surplus handle was dropped"
    );

    // Root only the **middle** payload; the outer two are unreachable.
    // SAFETY: the process's own handle keeps the payload alive and the 8-word pair fits the young
    // heap.
    let root =
        unsafe { refc_binary_term(process.heap_mut(), ptrs[1] as *mut u8, payload().len()) };
    root_in_registers(&mut process, &[root])?;

    assert_eq!(
        process.sweep_off_heap_binaries_reachable()?,
        2,
        "the two unrooted payloads were released"
    );
    assert!(process.off_heap_binary(slots[0]).is_none(), "slot 0 was released");
    assert!(process.off_heap_binary(slots[2]).is_none(), "slot 2 was released");
    let survivor = process
        .off_heap_binary(slots[1])
        .expect("the rooted payload keeps its original slot");
    assert_eq!(survivor.data_ptr() as usize, ptrs[1]);
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(alloc.stats().live_blocks, 1, "only the two unreferenced blocks were freed");

    // A fresh registration never reuses a released slot.
    let fresh = off_heap_payload(&alloc)?;
    assert_eq!(process.register_off_heap_binary(fresh), 3);
    assert!(process.off_heap_binary(slots[0]).is_none(), "slot 0 stays released");
    Ok(())
}

/// The declared-dead contract is honoured by the automatic reclamation step **even when the handle is
/// still on the heap**: the caller's assertion is authoritative, so the conservative half releases it
/// after the reachability half has classified it live. (The declaration is what the caller may base
/// on knowledge this crate cannot derive — e.g. a handle in a fragment about to be dropped.)
#[test]
fn a_declared_death_is_honoured_over_the_reachability_verdict() -> Result<()> {
    let alloc = test_allocator("offheap-reach-declared-wins");
    let binary = off_heap_payload(&alloc)?;
    let data_ptr = binary.data_ptr();

    let mut process = process(9);
    // The registration moves the sole handle into the process, so the asserted death really frees the
    // payload block.
    let slot = process.register_off_heap_binary(binary);
    // SAFETY: the process's own handle keeps the payload alive and the 8-word pair fits the young
    // heap.
    let root = unsafe { refc_binary_term(process.heap_mut(), data_ptr, payload().len()) };
    root_in_registers(&mut process, &[root])?;

    // The reachability half alone would keep it ...
    assert_eq!(process.sweep_off_heap_binaries_reachable()?, 0);

    // ... but the caller's declaration is authoritative: the collection's conservative half releases
    // it. No word of the payload is read after this point.
    assert!(process.declare_off_heap_binary_dead(slot));
    process.collect()?;
    assert!(process.off_heap_binary(slot).is_none());
    assert_eq!(process.off_heap_binary_count(), 0);
    assert_eq!(
        alloc.stats().live_blocks,
        0,
        "the asserted death freed the payload"
    );
    assert_eq!(alloc.stats().live_bytes, 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// (f) a relocation moves the handle, never the payload
// ---------------------------------------------------------------------------

/// **(f)** After a **real collection** relocates a young heap that holds a `[BinRef][ErlSubBits]`
/// pair, the payload is still retained — the handle moved to a new address, but the payload pointer
/// (`BinRef.val`, the frozen `data_ptr` contract) is unchanged, and the relocated handle still names
/// the very same off-heap payload.
#[test]
fn a_collection_that_relocates_the_handle_keeps_the_payload() -> Result<()> {
    let bytes = payload();
    let alloc = test_allocator("offheap-reach-relocated");
    let binary = off_heap_payload(&alloc)?;
    let data_ptr = binary.data_ptr();

    let mut process = process(10);
    let slot = process.register_off_heap_binary(binary.clone());
    let root = bin_ref_pair(&mut process, &binary);
    let handle_before = boxed_val(root) as usize;
    root_in_registers(&mut process, &[root])?;

    let collection = process.collect()?;
    assert!(
        collection.stats.words_copied >= ERL_REFC_BITS_SIZE,
        "the rooted `[BinRef][ErlSubBits]` pair was relocated"
    );

    // The payload survived the collection (the reachability half ran over the post-collection heap).
    let held = process.off_heap_binary(slot).expect("the payload survived the relocation");
    assert_eq!(held.data_ptr(), data_ptr, "the off-heap payload never moves");
    assert_eq!(held.as_bytes(), &bytes[..]);
    assert_eq!(held.refcount(), 2, "the reference count is consistent");
    assert_eq!(alloc.stats().live_blocks, 1);

    // The relocated handle names the same payload: the `ErlSubBits` root moved, its `orig` `BinRef`
    // moved with it, and the new `BinRef.val` still holds the unchanged payload pointer.
    let relocated = process.registers().x().get(0)?;
    assert!(
        unsafe { is_refc_binary(relocated) },
        "the relocated x(0) is still a refc binary"
    );
    assert_eq!(unsafe { refc_size(relocated) }, bytes.len() * 8);
    let handle_after = boxed_val(relocated) as usize;
    assert_ne!(
        handle_after, handle_before,
        "the collection really relocated the handle (it is not an in-place survivor)"
    );
    // SAFETY: `relocated` is a live refc binary (its `ErlSubBits`), so word 4 is its `orig` — a
    // boxed pointer to the copied `BinRef`.
    let orig = unsafe { *boxed_val(relocated).add(4) };
    // SAFETY: `orig` is the live copied `BinRef`, so its word 1 is the `val` slot.
    assert_eq!(
        unsafe { (*boxed_val(orig).add(1)).raw() as *mut u8 },
        data_ptr,
        "the relocated `BinRef.val` still points at the off-heap payload"
    );
    Ok(())
}
