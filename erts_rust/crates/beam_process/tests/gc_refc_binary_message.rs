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

//! Integration evidence that a safepoint collection preserves a queued message's **off-heap
//! ownership**.
//!
//! `Process::collect` drains the mailbox, pushes every payload as a root, runs the collection and
//! re-enqueues each drained envelope around its relocated payload. The rebuild must carry the
//! envelope's **whole** ownership: a message whose payload references an off-heap refc binary holds
//! a counted `beam_alloc::RefcBinary` handle in `Message::binaries`, and the on-heap
//! `[BinRef(3)][ErlSubBits(5)]` pair the collection relocates keeps pointing at that very off-heap
//! payload. Rebuilding the envelope from scratch (`Message::new` + re-applying the token/off-heap
//! builders) silently drops that handle; with the message the sole owner, dropping it frees the
//! shared payload, so the relocated `BinRef.val` ends up pointing at **freed** memory — a live
//! use-after-free (and, symmetrically, the envelope's `fragments`/`nodes` lists are lost).
//!
//! The assertions below therefore fail deterministically on the pre-fix rebuild — the queued
//! message's [`binaries()`](beam_message::Message::binaries) comes back **empty** — and pass once
//! the rebuild goes through `beam_message::Message::with_payload`, which replaces only the payload
//! term and keeps everything else.
//!
//! Determinism: the payload is a fixed 200-byte pattern (> `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes, so
//! its bytes really live in an off-heap counted object rather than in the on-heap `ErlHeapBits`
//! form); the collection is forced by an explicit `Process::collect` call, so no timing, sleep or
//! wall-clock bound is involved. The binary-handle assertion is checked **before** the payload is
//! read, so a pre-fix run never dereferences the freed bytes.

use beam_alloc::{Heap, RefcBinary};
use beam_common::Result;
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::prelude::{
    boxed_val, is_refc_binary, refc_bytes, refc_size, write_refc_binary, AtomTable, Eterm,
    ERL_ONHEAP_BINARY_LIMIT, ERL_REFC_BITS_SIZE,
};

/// A 200-byte payload whose bytes are position-dependent, so a shifted, duplicated or truncated
/// view of the relocated binary is visible.
fn payload() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// Writes the on-heap refc-binary pair for `data_ptr`/`len` and returns the tagged root (the
/// `ErlSubBits`).
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

/// An empty process table (only the pid table and the signal atoms are used here).
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A queued message whose payload references an off-heap refc binary keeps that payload alive
/// across a receiver collection: the rebuilt envelope still owns the counted binary handle, so the
/// relocated `BinRef.val` pointer stays valid and the bytes read back unchanged.
#[test]
fn a_collection_keeps_a_queued_refc_binary_messages_payload_alive() -> Result<()> {
    let bytes = payload();
    assert!(
        bytes.len() > ERL_ONHEAP_BINARY_LIMIT,
        "the payload must exceed the on-heap limit so it is an off-heap refc binary"
    );

    // The sender's arena: the on-heap `[BinRef][ErlSubBits]` pair, plus the off-heap handle the
    // copy engine resolves it through (`binary_by_data_ptr`).
    let mut src = Heap::new(64)?;
    let mut src_off = OffHeapTable::new();
    let source_binary = RefcBinary::new(&bytes)?;
    let data_ptr = source_binary.data_ptr();
    src_off.push_binary(source_binary);
    // SAFETY: `src_off` owns the payload for the whole test, and the 8-word pair fits `src`.
    let original = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    assert!(unsafe { is_refc_binary(original) }, "the term is a refc binary");
    assert_eq!(
        src_off.binaries()[0].refcount(),
        1,
        "the sender's table holds the single handle so far"
    );

    let mut table = table();
    let sender = table.spawn_default()?;
    let receiver = table.spawn_default()?;

    // The copy **shares** the payload: the copy engine refcounts it and the queued message takes
    // that handle on (`send` drains the destination table into the message).
    let outcome = table.deliver(receiver, sender.to_eterm(), original, &src_off)?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));
    {
        let queued = table
            .get(receiver)
            .expect("the receiver is live")
            .mailbox()
            .peek()
            .expect("one message is queued");
        assert_eq!(
            queued.binaries().len(),
            1,
            "the copy registered the shared off-heap payload on the message"
        );
        assert_eq!(
            queued.binaries()[0].data_ptr(),
            data_ptr,
            "the payload is shared, not deep-copied"
        );
        assert_eq!(
            queued.binaries()[0].refcount(),
            2,
            "the sender's table plus the queued message"
        );
        assert_eq!(
            queued.logical_binary_bytes(),
            bytes.len(),
            "the message accounts the shared payload's logical bytes"
        );
    }

    // Drop the sender's handle: from here the queued message is the **sole** owner of the payload,
    // exactly the state the runtime leaves behind once its transient send-table is gone.
    drop(src_off);
    assert_eq!(
        table
            .get(receiver)
            .expect("the receiver is live")
            .mailbox()
            .peek()
            .expect("one message is queued")
            .binaries()[0]
            .refcount(),
        1,
        "the queued message owns the payload on its own"
    );

    // Force a safepoint collection on the receiver while the message is queued: the payload (the
    // `[BinRef][ErlSubBits]` pair) is a root and is copied into the mature space.
    let collection = table.get_mut(receiver).expect("the receiver is live").collect()?;
    assert!(
        collection.stats.objects_copied >= 2,
        "the copied payload is at least the SubBits + the BinRef, got {}",
        collection.stats.objects_copied
    );

    // The message survived, in FIFO order.
    let message = table
        .get(receiver)
        .expect("the receiver is live")
        .mailbox()
        .peek()
        .expect("the message survived the collection");

    // (b) The envelope's off-heap ownership is intact — the deterministic pre-fix failure: the
    // from-scratch rebuild dropped `binaries`, so this is empty (and the payload already freed).
    assert_eq!(
        message.binaries().len(),
        1,
        "the rebuilt message still owns the shared refc binary"
    );
    assert_eq!(
        message.binaries()[0].data_ptr(),
        data_ptr,
        "the message still holds the very same off-heap payload"
    );
    assert_eq!(
        message.binaries()[0].refcount(),
        1,
        "the handle count is unchanged by the collection"
    );
    assert_eq!(message.logical_binary_bytes(), bytes.len());

    // (a) The relocated payload is still a readable refc binary with the original bytes, and its
    // `BinRef.val` still points at the (still-live) off-heap payload.
    let relocated = message.payload();
    assert!(
        unsafe { is_refc_binary(relocated) },
        "the relocated payload is still a refc binary"
    );
    assert_eq!(unsafe { refc_size(relocated) }, bytes.len() * 8);
    assert_eq!(
        unsafe { refc_bytes(relocated) },
        &bytes[..],
        "the off-heap payload bytes are unchanged"
    );
    // SAFETY: `relocated` is a live refc binary (its `ErlSubBits`), so word 4 is its `orig` — a
    // boxed pointer to the copied `BinRef` (the pair is copied as two objects, so it need not sit
    // immediately below the `ErlSubBits`).
    let orig = unsafe { *boxed_val(relocated).add(4) };
    // SAFETY: `orig` is the live copied `BinRef`, so its word 1 is the `val` slot.
    assert_eq!(
        unsafe { (*boxed_val(orig).add(1)).raw() as *mut u8 },
        data_ptr,
        "the relocated `BinRef.val` still points at the same off-heap payload"
    );

    Ok(())
}
