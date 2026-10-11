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

//! Integration evidence for the **process-owned off-heap refc-binary list** and its sweep.
//!
//! `beam_gc::OffHeap` is the workspace's slot-stable off-heap list, and `beam_gc`'s own handoff
//! note records that no production caller drove its `sweep`: liveness of an off-heap object is the
//! caller's concern, and `beam_message::OffHeapTable` (the process's message-transit table)
//! deliberately never sweeps. This file pins the process half of that handoff — `Process` now owns
//! a `beam_gc::OffHeap` of the refc binaries it takes ownership of
//! ([`Process::register_off_heap_binary`]), exposes it for reading
//! ([`Process::off_heap_binary`]/[`off_heap_binaries`]/[`off_heap_binary_count`]/
//! [`off_heap_binary_bytes`]), and **sweeps** it — automatically at the end of every rooted
//! collection (`run_rooted_collection`'s last step) and on demand
//! ([`Process::sweep_off_heap_binaries`]).
//!
//! # The predicate is conservative, and that is the point
//!
//! A payload is released only when it is **provably dead**. The process cannot decide liveness on
//! its own: the on-heap `BinRef` that names a payload is nested inside boxed terms, and walking
//! them is a collector's job (the walk is `pub(crate)` to `beam_gc`), so the only death this crate
//! may act on is the caller's **assertion**
//! ([`Process::declare_off_heap_binary_dead`], consumed by the next sweep). Everything else is
//! retained — a leak, never a dangle. [`Process::retain_off_heap_binaries`] is the seam for a
//! party that *can* decide liveness faithfully (a collector- or host-side heap scan).
//!
//! # Which assertions fail on the pre-change code
//!
//! "Old code" is HEAD before this change: `Process` had no owned off-heap list and no sweep on its
//! path at all, so *every* test here fails to compile against it (the registration/read/sweep API
//! does not exist). Where a runtime assertion is the more precise witness it is called out below
//! (the released-count and slot-resolution assertions have no old-code counterpart because there
//! was no list to release from).
//!
//! Determinism: everything is forced by explicit API calls — a fixed 200-byte payload (>
//! `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes, so its bytes really live in an off-heap counted object),
//! an **injected** `AllocRef` so the payload's block/byte accounting is observable in isolation,
//! and explicit `collect()`/`sweep` calls. No sleep, no wall-clock bound, no oracle.

use beam_alloc::{AllocatorConfig, AllocRef, CarrierAllocator, Heap, RefcBinary};
use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{
    boxed_val, is_refc_binary, refc_bytes, refc_size, write_refc_binary, Eterm,
    ERL_ONHEAP_BINARY_LIMIT, ERL_REFC_BITS_SIZE,
};

/// A 200-byte payload whose bytes are position-dependent, so a shifted, duplicated or truncated
/// view of the payload is visible.
fn payload() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// An isolated carrier allocator, so a test can observe a binary's payload block being freed
/// through `AllocStats` without racing the process-wide registry allocator.
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

/// A fresh process with a normal heap.
fn process(pid: u64) -> Process {
    Process::new(ProcessId::new(pid, 0)).expect("the default heap allocates")
}

/// **(Acceptance 1)** A live off-heap payload survives **both** a sweep and a process collection —
/// byte-intact, with a consistent (neither over- nor under-decremented) reference count.
///
/// The payload's bytes are also named by a real on-heap `[BinRef][ErlSubBits]` pair rooted in
/// `x(0)`, so the collection relocates a genuine reference to it, and a second counted handle is
/// held outside the process (the "another owner" arm of the acceptance criterion).
#[test]
fn a_live_owned_binary_survives_a_sweep_and_a_collection() -> Result<()> {
    let bytes = payload();
    assert!(
        bytes.len() > ERL_ONHEAP_BINARY_LIMIT,
        "the payload must exceed the on-heap limit so it is an off-heap refc binary"
    );
    let alloc = test_allocator("offheap-binaries-live");
    let binary = off_heap_payload(&alloc)?;
    let data_ptr = binary.data_ptr();
    assert_eq!(alloc.stats().live_blocks, 1, "the payload block is live");

    let mut process = process(1);
    // The process takes ownership of one handle; the local `binary` is the second owner.
    let slot = process.register_off_heap_binary(binary.clone());
    assert_eq!(slot, 0, "the first registration lands at slot 0");
    assert_eq!(
        process.off_heap_binary(slot).expect("slot 0 resolves").refcount(),
        2,
        "the process's handle plus the local owner"
    );
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(process.off_heap_binary_bytes(), bytes.len());

    // A genuine on-heap reference to the payload, rooted in `x(0)`.
    // SAFETY: `binary` (and the process's handle) keep the payload alive for the whole test, and
    // the 8-word pair fits the young heap.
    let root = unsafe { refc_binary_term(process.heap_mut(), data_ptr, bytes.len()) };
    process.registers_mut().x_mut().set_live(1)?;
    process.registers_mut().x_mut().set(0, root)?;

    // (a) A sweep with nothing declared dead releases nothing — the conservative predicate.
    assert_eq!(
        process.sweep_off_heap_binaries(),
        0,
        "no entry was declared dead, so the sweep releases nothing"
    );

    // (b) A collection relocates the referencing pair and drives the same sweep internally; the
    //     payload is untouched because it is off the heap.
    let collection = process.collect()?;
    assert!(
        collection.stats.words_copied >= ERL_REFC_BITS_SIZE,
        "the rooted `[BinRef][ErlSubBits]` pair was relocated"
    );

    // (c) The payload is byte-intact and its count is unchanged (2) — neither decremented (a
    //     wrongly-released entry) nor incremented (an accidental extra clone).
    let held = process.off_heap_binary(slot).expect("the entry survived");
    assert_eq!(held.as_bytes(), &bytes[..], "the payload bytes are unchanged");
    assert_eq!(
        held.refcount(),
        2,
        "the reference count is consistent across the sweep and the collection"
    );
    assert_eq!(held.data_ptr(), data_ptr);
    assert_eq!(process.off_heap_binary_count(), 1, "nothing was released");
    assert_eq!(process.off_heap_binary_bytes(), bytes.len());
    assert_eq!(alloc.stats().live_blocks, 1, "the payload block is still live");
    assert!(
        alloc.stats().live_bytes >= bytes.len(),
        "the payload block is charged to the allocator"
    );

    // (d) The relocated on-heap reference still names the very same off-heap payload.
    let relocated = process.registers().x().get(0)?;
    assert!(
        unsafe { is_refc_binary(relocated) },
        "the relocated x(0) is still a refc binary"
    );
    assert_eq!(unsafe { refc_size(relocated) }, bytes.len() * 8);
    assert_eq!(unsafe { refc_bytes(relocated) }, &bytes[..]);
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

/// **(Acceptance 2)** A genuinely dropped entry is released **exactly once**: its payload is freed
/// at reference count 0 (observed through the injected allocator's `live_blocks`/`live_bytes`), a
/// second sweep of the same slot releases nothing, and its destructor never runs again.
#[test]
fn a_declared_dead_owned_binary_is_released_exactly_once() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-dead");
    let binary = off_heap_payload(&alloc)?;
    let live_bytes = alloc.stats().live_bytes;
    assert_eq!(alloc.stats().live_blocks, 1);

    let mut process = process(2);
    // The registration **moves** the sole handle into the process: refcount stays 1.
    let slot = process.register_off_heap_binary(binary);
    assert_eq!(
        process.off_heap_binary(slot).expect("slot 0 resolves").refcount(),
        1,
        "the process holds the only handle"
    );
    assert_eq!(alloc.stats().live_blocks, 1);

    // The caller asserts the payload is dead — the one provable-death signal.
    assert!(
        process.declare_off_heap_binary_dead(slot),
        "a live slot can be declared dead"
    );
    // The declaration is deferred: nothing is released until the sweep.
    assert_eq!(process.off_heap_binary_count(), 1, "still owned before the sweep");

    // The released-count assertion (there is no old-code counterpart: the pre-change `Process` had
    // no off-heap list and no sweep).
    assert_eq!(
        process.sweep_off_heap_binaries(),
        1,
        "the sweep released the declared-dead entry"
    );
    assert_eq!(
        alloc.stats().live_blocks,
        0,
        "the payload block was freed at reference count 0"
    );
    assert_eq!(alloc.stats().live_bytes, 0);
    assert!(
        live_bytes > 0,
        "the payload really held bytes, so the drop is observable"
    );
    assert!(process.off_heap_binary(slot).is_none(), "the slot is released");
    assert_eq!(process.off_heap_binary_count(), 0);
    assert_eq!(process.off_heap_binary_bytes(), 0);

    // No double free: a second sweep releases nothing (the released slot is skipped), the allocator
    // stays at its baseline and the destructor cannot have run again.
    assert_eq!(
        process.sweep_off_heap_binaries(),
        0,
        "the already-released slot is never released a second time"
    );
    assert_eq!(alloc.stats().live_blocks, 0, "still no live block");
    assert_eq!(alloc.stats().live_bytes, 0);
    assert!(
        !process.declare_off_heap_binary_dead(slot),
        "a released slot cannot be declared dead again"
    );
    assert_eq!(process.sweep_off_heap_binaries(), 0);
    Ok(())
}

/// A sweep releases only the process's **own** handle: a payload another owner still holds (here a
/// second `RefcBinary` clone, the shape of a `Message`'s `binaries()` handle) survives, at the
/// correctly reduced count — the "no over-decrement" half of the acceptance criterion.
#[test]
fn a_sweep_never_frees_a_payload_another_owner_still_holds() -> Result<()> {
    let bytes = payload();
    let alloc = test_allocator("offheap-binaries-shared");
    let binary = off_heap_payload(&alloc)?;
    let data_ptr = binary.data_ptr();

    let mut process = process(3);
    let slot = process.register_off_heap_binary(binary.clone());
    assert_eq!(process.off_heap_binary(slot).expect("live").refcount(), 2);

    // The process returns its ownership of the shared payload ...
    assert!(process.declare_off_heap_binary_dead(slot));
    assert_eq!(process.sweep_off_heap_binaries(), 1);
    assert!(process.off_heap_binary(slot).is_none(), "the entry is released");
    assert_eq!(process.off_heap_binary_count(), 0);

    // ... but the other owner keeps it alive: one handle, one live block, the bytes intact.
    assert_eq!(binary.refcount(), 1, "the surviving owner's handle alone remains");
    assert_eq!(binary.data_ptr(), data_ptr);
    assert_eq!(binary.as_bytes(), &bytes[..], "the payload was not freed");
    assert_eq!(alloc.stats().live_blocks, 1, "the payload block is still live");
    Ok(())
}

/// **(Acceptance 3)** Slot indices stay valid across a sweep: a surviving entry keeps its original
/// slot (intact), the released slot resolves to `None` — never to an unrelated shifted-in entry,
/// because the list never shifts, reorders or reuses a slot — and a fresh registration appends at a
/// new index.
#[test]
fn slot_indices_stay_valid_across_a_sweep() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-slots");

    let mut process = process(4);
    let mut ptrs = Vec::new();
    for index in 0..3usize {
        let binary = off_heap_payload(&alloc)?;
        ptrs.push(binary.data_ptr() as usize);
        // Each payload is moved into the process, so the list is its **only** owner and releasing
        // the entry really frees it.
        assert_eq!(
            process.register_off_heap_binary(binary),
            index,
            "registrations append at fresh slots 0, 1, 2"
        );
    }
    assert_eq!(process.off_heap_binary_count(), 3);
    assert_eq!(process.off_heap_binary_bytes(), 3 * payload().len());
    assert_eq!(alloc.stats().live_blocks, 3);

    // Release the **middle** entry only.
    assert!(process.declare_off_heap_binary_dead(1));
    assert_eq!(process.sweep_off_heap_binaries(), 1);
    assert_eq!(process.off_heap_binary_count(), 2);
    assert_eq!(process.off_heap_binary_bytes(), 2 * payload().len());
    assert_eq!(alloc.stats().live_blocks, 2, "only the middle payload was freed");

    // The survivors are still at their **original** slots, intact and still owned.
    for survivor in [0usize, 2] {
        let entry = process
            .off_heap_binary(survivor)
            .unwrap_or_else(|| panic!("slot {survivor} must survive at its original index"));
        assert_eq!(entry.data_ptr() as usize, ptrs[survivor], "slot {survivor} identity");
        assert_eq!(entry.as_bytes(), &payload()[..], "slot {survivor} payload intact");
    }
    // The released slot answers `None` — never an unrelated shifted-in survivor.
    assert!(
        process.off_heap_binary(1).is_none(),
        "the released slot must not resolve to another entry"
    );
    assert!(process.off_heap_binary(3).is_none(), "out of range");
    assert_eq!(
        process.off_heap_binaries().count(),
        process.off_heap_binary_count()
    );

    // A further registration appends at a **fresh** index, leaving the released slot released.
    let fresh = off_heap_payload(&alloc)?;
    assert_eq!(
        process.register_off_heap_binary(fresh),
        3,
        "a fresh registration never reuses the released slot"
    );
    assert!(process.off_heap_binary(1).is_none(), "slot 1 stays released");
    assert_eq!(process.off_heap_binary_count(), 3);
    Ok(())
}

/// The sweep runs **on the process path**: a rooted collection reclaims a payload the caller
/// declared dead, with no explicit `sweep_off_heap_binaries` call — and does not touch an
/// undeclared one beside it.
#[test]
fn a_collection_drives_the_off_heap_sweep() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-collection");
    let doomed = off_heap_payload(&alloc)?;
    let kept = off_heap_payload(&alloc)?;
    assert_eq!(alloc.stats().live_blocks, 2);

    let mut process = process(5);
    let doomed_slot = process.register_off_heap_binary(doomed);
    let kept_slot = process.register_off_heap_binary(kept.clone());
    assert!(process.declare_off_heap_binary_dead(doomed_slot));

    // No explicit sweep: the collection's last step is the sweep.
    process.collect()?;

    assert!(
        process.off_heap_binary(doomed_slot).is_none(),
        "the collection reclaimed the declared-dead entry"
    );
    let survivor = process.off_heap_binary(kept_slot).expect("the undeclared entry survived");
    assert_eq!(survivor.data_ptr(), kept.data_ptr());
    assert_eq!(survivor.as_bytes(), &payload()[..]);
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(alloc.stats().live_blocks, 1, "exactly the declared-dead payload was freed");
    Ok(())
}

/// Registering a payload the process already owns returns its **existing** slot (the `MSO(p)`
/// one-reference-per-object invariant) and drops the surplus handle, so the reference count is
/// unchanged — and a declared death therefore names exactly one slot.
#[test]
fn registering_a_payload_twice_keeps_one_slot_and_one_reference() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-dedup");
    let binary = off_heap_payload(&alloc)?;

    let mut process = process(6);
    let first = process.register_off_heap_binary(binary.clone());
    let second = process.register_off_heap_binary(binary.clone());
    assert_eq!(first, second, "the payload keeps its existing slot");
    assert_eq!(process.off_heap_binary_count(), 1, "one entry per payload");
    assert_eq!(
        process.off_heap_binary(first).expect("live").refcount(),
        2,
        "the surplus registration dropped its handle: the local owner plus the list's one"
    );
    assert_eq!(process.off_heap_binary_bytes(), payload().len());

    // One declaration releases the single slot.
    assert!(process.declare_off_heap_binary_dead(first));
    assert_eq!(process.sweep_off_heap_binaries(), 1);
    assert_eq!(process.off_heap_binary_count(), 0);
    assert_eq!(binary.refcount(), 1, "the local owner's handle remains");
    Ok(())
}

/// A process's `Drop` releases every payload it still owns — the process-death reclamation path for
/// entries no caller declared dead (the conservative fallback's endpoint: a leak, never a dangle).
#[test]
fn a_dropped_process_releases_the_binaries_it_still_owns() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-drop");
    let binary = off_heap_payload(&alloc)?;
    assert_eq!(alloc.stats().live_blocks, 1);

    {
        let mut process = process(7);
        process.register_off_heap_binary(binary.clone());
        assert_eq!(process.off_heap_binary_count(), 1);
    }

    assert_eq!(
        binary.refcount(),
        1,
        "the process's handle was released on drop"
    );
    assert_eq!(alloc.stats().live_blocks, 1, "the local owner keeps the payload alive");

    drop(binary);
    assert_eq!(alloc.stats().live_blocks, 0, "the last handle freed the payload");
    assert_eq!(alloc.stats().live_bytes, 0);
    Ok(())
}

/// [`Process::retain_off_heap_binaries`] is the seam for a party that **can** decide liveness (a
/// collector- or host-side heap scan): the predicate is applied to every live entry, survivors keep
/// their slots and everything the predicate reports live is retained untouched.
#[test]
fn a_faithful_liveness_predicate_can_drive_the_sweep() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-predicate");
    let a = off_heap_payload(&alloc)?;
    let b = off_heap_payload(&alloc)?;
    let b_ptr = b.data_ptr() as usize;
    let a_ptr = a.data_ptr() as usize;

    let mut process = process(8);
    let a_slot = process.register_off_heap_binary(a.clone());
    let b_slot = process.register_off_heap_binary(b.clone());

    // A "scan" that only ever found the payload at `b_ptr` still reachable.
    let releases = process.retain_off_heap_binaries(|binary| binary.data_ptr() as usize == b_ptr);
    assert_eq!(releases, 1, "only the payload the scan did not find was released");
    assert!(
        process.off_heap_binary(a_slot).is_none(),
        "the scan-reported-dead entry was released"
    );
    let survivor = process.off_heap_binary(b_slot).expect("the reported-live entry survived");
    assert_eq!(survivor.data_ptr() as usize, b_ptr);
    assert_eq!(
        survivor.refcount(),
        2,
        "the scan-driven sweep left the process's live handle alone"
    );
    assert_eq!(a.refcount(), 1, "the released entry dropped its handle");
    assert_eq!(a.data_ptr() as usize, a_ptr);
    assert_eq!(alloc.stats().live_blocks, 2, "both payloads are still allocated");
    Ok(())
}

/// A collection leaves the owned off-heap list alone when nothing is declared dead — the
/// conservative default that makes the sweep safe to run on every collection.
#[test]
fn a_collection_without_a_declaration_releases_nothing() -> Result<()> {
    let alloc = test_allocator("offheap-binaries-conservative");
    let binary = off_heap_payload(&alloc)?;

    let mut process = process(9);
    let slot = process.register_off_heap_binary(binary.clone());
    let before = process.off_heap_binary_bytes();

    process.collect()?;
    process.collect()?;

    let held = process.off_heap_binary(slot).expect("the entry survived both collections");
    assert_eq!(held.as_bytes(), &payload()[..]);
    assert_eq!(held.refcount(), 2);
    assert_eq!(process.off_heap_binary_count(), 1);
    assert_eq!(process.off_heap_binary_bytes(), before);
    assert_eq!(alloc.stats().live_blocks, 1);
    Ok(())
}