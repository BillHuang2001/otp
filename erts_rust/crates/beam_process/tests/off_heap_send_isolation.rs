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

//! Integration evidence that a **send never drains the receiver's persistent off-heap table**.
//!
//! `beam_message::send` (`enqueue_copied`) is designed around a per-send **staging** table: it
//! drains the copy's destination off-heap table into the [`Message`](beam_message::Message) it
//! builds, because that destination *is* the message's off-heap ownership. The destination must
//! therefore start empty — passing `Process.off_heap` (the process's **persistent** table) makes
//! every send *steal* whatever the process already had registered there: most importantly a queued
//! signal's reason, which [`Process::copy_into_heap`](beam_process::Process) registers in
//! `Process.off_heap` and leaves there for the process's lifetime (its on-heap
//! `PendingSignal.reason` references those slots). The queued signal is then left pointing at freed
//! slots/payloads — a use-after-free.
//!
//! [`a_send_leaves_the_receivers_persistent_off_heap_table_intact`] pins the fix white-box: seed the
//! process's persistent table, send to that very process, and assert the seed is **still** there (on
//! the pre-fix code the send drains it into the message, so the assertion fails) and that the
//! message owns only what it copied (nothing — the payload is an immediate).
//!
//! [`a_send_still_owns_exactly_what_it_copied`] closes the other side: with a **fresh** staging
//! table a payload that really references an off-heap object is still registered on the message
//! (and only on the message), so the fix separates the two tables rather than breaking off-heap
//! capture.
//!
//! Determinism: everything is forced by explicit API calls — no sleep, no wall-clock bound and no
//! oracle. The 200-byte payload exceeds `ERL_ONHEAP_BINARY_LIMIT` (64 bytes), so it is a real
//! off-heap counted object rather than the on-heap `ErlHeapBits` form.

use beam_alloc::{Heap, RefcBinary};
use beam_common::Result;
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::prelude::{
    make_small, write_refc_binary, Eterm, ERL_ONHEAP_BINARY_LIMIT, ERL_REFC_BITS_SIZE,
};

/// A 200-byte payload (> the on-heap binary limit) whose bytes are position-dependent, so a
/// shifted, duplicated or truncated view of the binary is visible.
fn payload() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// Writes the on-heap `[BinRef][ErlSubBits]` pair for `data_ptr`/`len` and returns the tagged root
/// (the `ErlSubBits`).
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

/// A send to a process that already holds a persistent off-heap object must not take that object
/// away from the process: the copy stages through a fresh table of its own.
#[test]
fn a_send_leaves_the_receivers_persistent_off_heap_table_intact() -> Result<()> {
    let bytes = payload();
    assert!(
        bytes.len() > ERL_ONHEAP_BINARY_LIMIT,
        "the payload must exceed the on-heap limit so it is a real off-heap object"
    );

    // A standalone receiver PCB (no pid table needed: this is the single-process send path).
    let mut process = Process::new(ProcessId::new(1, 0))?;

    // Seed the process's PERSISTENT off-heap table — standing in for a queued signal's reason, the
    // other producer being `Process::heap_and_off_heap_mut`'s host-registered fragment objects.
    let persistent = RefcBinary::new(&bytes)?;
    let data_ptr = persistent.data_ptr();
    assert_eq!(process.off_heap_mut().push_binary(persistent), 0);
    assert_eq!(
        process.off_heap().binaries().len(),
        1,
        "the persistent object is registered before the send"
    );
    assert_eq!(
        process.off_heap().binaries()[0].refcount(),
        1,
        "the process is the sole owner so far"
    );

    // Deliver to this very process. The payload is an immediate, so the message it builds owns no
    // off-heap object of its own.
    let outcome = process.deliver_simple(ProcessId::new(2, 0).to_eterm(), make_small(1))?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));

    // (1) The send must NOT have stolen the persistent object. On the pre-fix code `send` drains
    //     the process's own table into the message, so this count is 0 and the assertion fails.
    assert_eq!(
        process.off_heap().binaries().len(),
        1,
        "the persistent off-heap object survives the send"
    );
    assert_eq!(
        process.off_heap().binaries()[0].data_ptr(),
        data_ptr,
        "the very same payload is still held by the process"
    );
    assert_eq!(
        process.off_heap().binaries()[0].refcount(),
        1,
        "the send added no reference to the persistent object"
    );

    // (2) The message owns only what it copied: the immediate payload references nothing off-heap,
    //     so the message's off-heap lists are empty (pre-fix they hold the stolen object).
    let message = process.mailbox().peek().expect("the message was enqueued");
    assert_eq!(message.payload(), make_small(1));
    assert!(
        message.off_heap().is_empty(),
        "the message copied no MSO handle of its own"
    );
    assert!(
        message.binaries().is_empty(),
        "the message did not steal the process's persistent binary"
    );

    Ok(())
}

/// The complement: a payload that really references an off-heap object is still captured on the
/// **message** (and only there) — the fix separates the staging table from the persistent one
/// without losing off-heap capture.
#[test]
fn a_send_still_owns_exactly_what_it_copied() -> Result<()> {
    let bytes = payload();

    // The sender's arena: the on-heap `[BinRef][ErlSubBits]` pair plus the off-heap handle the copy
    // engine resolves it through (`binary_by_data_ptr`).
    let mut src = Heap::new(64)?;
    let mut src_off = OffHeapTable::new();
    let source_binary = RefcBinary::new(&bytes)?;
    let source_ptr = source_binary.data_ptr();
    src_off.push_binary(source_binary);
    // SAFETY: `src_off` owns the payload for the whole test and the 8-word pair fits `src`.
    let original = unsafe { refc_binary_term(&mut src, source_ptr, bytes.len()) };

    let mut process = Process::new(ProcessId::new(1, 0))?;

    // A pre-existing persistent object that must be left alone by the send.
    let persistent = RefcBinary::new(&[0u8; 200])?;
    let persistent_ptr = persistent.data_ptr();
    process.off_heap_mut().push_binary(persistent);

    let outcome = process.deliver(ProcessId::new(2, 0).to_eterm(), original, &src_off)?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));

    // The process's persistent object is untouched ...
    assert_eq!(process.off_heap().binaries().len(), 1);
    assert_eq!(process.off_heap().binaries()[0].data_ptr(), persistent_ptr);

    // ... while the message owns exactly the one object its payload references — the copy shared the
    // sender's payload (refcount++), not the process's persistent binary.
    let message = process.mailbox().peek().expect("the message was enqueued");
    assert_eq!(
        message.binaries().len(),
        1,
        "the message owns exactly the object its payload references"
    );
    assert_eq!(
        message.binaries()[0].data_ptr(),
        source_ptr,
        "the message holds the copied (shared) payload, not the persistent one"
    );
    assert_eq!(
        message.binaries()[0].refcount(),
        2,
        "the sender's table plus the queued message"
    );
    assert_eq!(message.logical_binary_bytes(), bytes.len());

    Ok(())
}
