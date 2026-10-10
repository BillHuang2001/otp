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

//! Regression evidence that a collection **re-installs a queued message with its whole
//! envelope**, not just its payload word.
//!
//! [`Process::collect`] / [`Process::ensure_capacity`] drain the mailbox, push every payload
//! as a GC root, run the collector and put the messages back. The message put back must be the
//! *same envelope* with a new payload word ([`Message::with_payload`]); a rebuild from scratch
//! (`Message::new(...)` plus the `from`/`token`/off-heap builders) silently **drops** the
//! fragment chain, the refc-binary list and the node list the message owns. For a
//! fragment-resident payload — one too large for the receiver's heap, whose term points *into*
//! the message's [`Fragments`] rather than into the nursery being collected — dropping the chain
//! frees the payload's home while the queued term still points there: a use-after-free. For a
//! heap-resident payload the dropped refc-binary handle releases a shared off-heap payload whose
//! on-heap `BinRef` still points at it.
//!
//! What this file pins, one guarantee per test:
//!
//! 1. [`a_fragment_resident_message_survives_a_collection_with_its_fragment_chain`] — the
//!    `Fragments` are still there after the collection, the payload is still *inside* them (it
//!    is not relocated, because it is outside every evacuated range) and it is still readable
//!    element by element.
//! 2. [`an_off_heap_payload_keeps_its_binary_and_node_lists_across_a_collection`] — a message
//!    that owns a shared refc binary and a node reference keeps both, exactly once each (the
//!    handle is neither dropped early — which would free the shared payload — nor registered
//!    twice — which would double free or leak), and the payload's `BinRef` still resolves to the
//!    live shared bytes.
//! 3. [`a_collection_preserves_mailbox_fifo_order_across_mixed_message_kinds`] — the FIFO order
//!    of a mixed heap-resident / fragment-resident mailbox is unchanged by the collection.
//! 4. [`an_exempt_process_refuses_the_collection_and_leaves_the_queued_message_untouched`] — the
//!    relocation-exempt path is untouched by the re-install: no collection runs, so the queued
//!    packet is exactly as it was.
//!
//! Determinism: every test forces the collection by *filling the nursery exactly* with unrooted
//! garbage (the `tests/exhaustion.rs` technique), so no timing is involved; every payload is
//! built on a plain [`Heap`] arena so the default nursery never limits the source graph. The
//! first assertion of each collecting test — `fragments()` / `binaries()` non-empty — is what
//! fails on a from-scratch rebuild, and it is asserted **before** anything dereferences the
//! payload a rebuild would have left dangling.

use beam_alloc::{Heap, RefcBinary};
use beam_common::{BeamError, Result};
use beam_message::{Fragments, HeapFragment, Message, OffHeapTable};
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    make_atom, make_small, refc_bytes, signed_value, tuple_element, tuple_size, write_external_pid,
    write_refc_binary, AtomTable, Eterm, ERL_REFC_BITS_SIZE,
};

/// A pid with the given number and serial 0 (the process table is not involved in these tests).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// Fills `process`'s nursery **completely** with unrooted garbage, so the collection that follows
/// has something to reclaim and provably ran (`words_reclaimed > 0`).
fn fill_nursery(process: &mut Process) -> Result<()> {
    // Whatever the already-delivered messages left live on the nursery is kept: the filler is
    // sized to the room that is left, never to the whole capacity.
    let available = process.collector().young().available_words();
    assert!(available >= 1, "the nursery has room for the filler's header");
    // A tuple of `available - 1` elements occupies `available` words: the arityval header plus the
    // elements. Unrooted, so the collection reclaims all of it.
    let filler = vec![make_small(0); available - 1];
    process.heap_mut().tuple(&filler)?;
    assert_eq!(process.collector().young().available_words(), 0);
    Ok(())
}

/// Copies `term` (built on a plain arena) into a one-fragment chain and returns the envelope a
/// sender's oversized-payload path would enqueue: a message whose payload points *into* the
/// fragment and which owns that fragment chain.
fn fragmented_message(term: Eterm, from: Eterm, capacity_words: usize) -> Result<Message> {
    let mut fragment = HeapFragment::new(capacity_words)?;
    let root = fragment.copy_root(term, &OffHeapTable::new())?;
    assert!(
        fragment.contains(root),
        "the payload root lives in the fragment, not on any process heap"
    );
    let mut fragments = Fragments::new();
    fragments.push(fragment);
    Ok(Message::new(root, from).with_fragments(fragments))
}

/// A fragment-resident queued payload survives the collection **with its fragment chain**: the
/// term still lives inside the message's `Fragments` (which the collector classified `Foreign`
/// and therefore left untouched) and is still readable element by element.
#[test]
fn a_fragment_resident_message_survives_a_collection_with_its_fragment_chain() -> Result<()> {
    let mut process = Process::new(pid(1))?;
    let mut arena = Heap::new(64)?;
    let source = arena.tuple(&[make_small(11), make_small(22), make_small(33)])?;

    let from = pid(2).to_eterm();
    process
        .mailbox_mut()
        .enqueue(fragmented_message(source, from, 64)?);
    assert_eq!(process.mailbox().len(), 1);

    fill_nursery(&mut process)?;
    let collection = process.collect()?;
    assert!(
        collection.stats.words_reclaimed > 0,
        "the unrooted filler was reclaimed, so a collection really ran"
    );

    let message = process
        .mailbox()
        .peek()
        .expect("the message is still queued after the collection");
    // (a) The fragment chain still belongs to the message. A from-scratch rebuild drops it here.
    let fragments = message
        .fragments()
        .expect("the message still owns its fragment chain");
    assert_eq!(fragments.len(), 1, "the chain is carried whole");
    let fragment = fragments.iter().next().expect("one fragment");
    // (b) The payload is still *fragment-resident*: the collector left it exactly where it was.
    assert!(
        fragment.contains(message.payload()),
        "the payload still points into the fragment (never relocated into the nursery)"
    );
    // (c) The envelope's scalar fields travel too.
    assert_eq!(message.from(), from, "the sender identity survived");
    // (d) The payload itself is intact and readable.
    // SAFETY: (b) proved the payload points into the live, message-owned fragment, and the root
    // was written as a 3-element tuple by `copy_root`; slots 0..3 are in bounds.
    unsafe {
        assert_eq!(tuple_size(message.payload()), 3);
        assert_eq!(tuple_element(message.payload(), 0), make_small(11));
        assert_eq!(tuple_element(message.payload(), 1), make_small(22));
        assert_eq!(tuple_element(message.payload(), 2), make_small(33));
    }
    assert!(compare::eq(source, message.payload(), &AtomTable::new()));
    Ok(())
}

/// A queued message that owns a shared refc binary **and** a node reference keeps both across the
/// collection, exactly once each, and its payload's `BinRef` still resolves to the live shared
/// bytes.
///
/// The payload is delivered through the real copy path ([`Process::deliver`] with a populated
/// source off-heap table), so the message genuinely owns the copied handle: the source
/// `BinRef.val` is resolved back to the source table's [`RefcBinary`] and cloned into the
/// message's binary list, and the copied external pid registers its node.
#[test]
fn an_off_heap_payload_keeps_its_binary_and_node_lists_across_a_collection() -> Result<()> {
    let mut process = Process::new(pid(1))?;
    let mut arena = Heap::new(128)?;

    // The off-heap payload the copied binary shares, and the source table that owns it (the copy
    // resolves the on-heap `BinRef.val` back to this handle).
    let bytes = b"a shared refc payload".to_vec();
    let binary = RefcBinary::new(&bytes)?;
    assert_eq!(binary.refcount(), 1, "the test's own handle");
    let mut src_off_heap = OffHeapTable::new();
    src_off_heap.push_binary(binary.clone());

    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    // The source payload `{Binary, ExternalPid}`.
    // SAFETY: `alloc_words` returned exactly the words `write_refc_binary` requires — 8
    // exclusively-owned words in `arena` (a bump heap that never reallocates, so the term stays
    // valid) — the `ErlSubBits` window covers exactly the off-heap payload, which `binary` and
    // `src_off_heap` keep alive for the whole test, and `null` is the empty off-heap chain link.
    let binary_term = unsafe {
        let hp = arena.alloc_words(ERL_REFC_BITS_SIZE)?;
        write_refc_binary(
            hp,
            binary.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            binary.data_ptr(),
            0,
            bytes.len() * 8,
        )
    };
    // SAFETY: `alloc_words` returned the 4-word `[header, node, creation, data]` block
    // `write_external_pid` requires, exclusively owned by `arena`; `node` is a live atom term.
    let external_pid = unsafe {
        let hp = arena.alloc_words(4)?;
        write_external_pid(hp, node, 0xAAAA_BBBB, 0xCCCC_DDDD, 7)
    };
    let payload = arena.tuple(&[binary_term, external_pid])?;

    let from = pid(2).to_eterm();
    process.deliver(from, payload, &src_off_heap)?;
    assert_eq!(process.mailbox().len(), 1);

    fill_nursery(&mut process)?;
    let collection = process.collect()?;
    assert!(collection.stats.words_reclaimed > 0, "a collection really ran");

    let message = process
        .mailbox()
        .peek()
        .expect("the message is still queued after the collection");
    // (a) The shared binary is still owned by the message — a from-scratch rebuild drops the list
    // here, releasing the handle and leaving the payload's `BinRef` pointing at a shared payload
    // nobody owns.
    let binaries = message.binaries();
    assert_eq!(binaries.len(), 1, "the message still owns its refc binary");
    assert_eq!(
        binaries[0].refcount(),
        3,
        "one handle each for the test, the source table and the message — neither dropped nor \
         registered twice"
    );
    assert_eq!(
        binaries[0].as_bytes(),
        &bytes[..],
        "the shared payload is untouched"
    );
    // (b) The node reference is still owned by the message, with exactly its one reference.
    let nodes = message.nodes();
    assert_eq!(nodes.len(), 1, "the message still owns its node reference");
    assert_eq!(
        nodes[0].refcount(),
        1,
        "one reference registered for the copied external pid, not two"
    );
    assert!(
        nodes[0].identity_eq(node, 7),
        "the node identity is the copied external pid's"
    );
    // (c) The relocated payload's `BinRef` still resolves to the live shared bytes.
    // SAFETY: the payload is the copied 2-element tuple `{Binary, ExternalPid}`; element 0 was
    // copied as a live 8-word refc-binary pair whose off-heap payload `binary` keeps alive for
    // the whole test, so the window's bytes are readable.
    unsafe {
        assert_eq!(tuple_size(message.payload()), 2);
        assert_eq!(
            refc_bytes(tuple_element(message.payload(), 0)),
            &bytes[..],
            "the payload still reads back the shared off-heap bytes"
        );
    }
    Ok(())
}

/// A collection leaves the mailbox's FIFO order alone for a mixed queue: heap-resident and
/// fragment-resident payloads interleave exactly as they were enqueued, and each keeps its own
/// envelope state.
#[test]
fn a_collection_preserves_mailbox_fifo_order_across_mixed_message_kinds() -> Result<()> {
    let mut process = Process::new(pid(1))?;
    let mut arena = Heap::new(64)?;

    let first = arena.tuple(&[make_small(1)])?;
    process.deliver_simple(pid(2).to_eterm(), first)?;

    let second = arena.tuple(&[make_small(2)])?;
    process
        .mailbox_mut()
        .enqueue(fragmented_message(second, pid(3).to_eterm(), 8)?);

    let third = arena.tuple(&[make_small(3)])?;
    process.deliver_simple(pid(4).to_eterm(), third)?;
    assert_eq!(process.mailbox().len(), 3);

    fill_nursery(&mut process)?;
    let collection = process.collect()?;
    assert!(collection.stats.words_reclaimed > 0, "a collection really ran");

    // The FIFO order is the enqueue order, and each payload still matches its own source term.
    let expected = [first, second, third];
    for (index, source) in expected.iter().enumerate() {
        let message = process
            .mailbox()
            .iter()
            .nth(index)
            .expect("the message is still queued");
        assert!(
            compare::eq(*source, message.payload(), &AtomTable::new()),
            "message {index} kept its payload and its position"
        );
        assert_eq!(
            message.fragments().is_some(),
            index == 1,
            "only the middle (fragment-resident) message carries fragments"
        );
    }
    assert_eq!(process.mailbox().len(), 3, "nothing was added or lost");

    // The fragment-resident middle message is still readable out of its own chain.
    // SAFETY: the middle message's payload points into its live, message-owned fragment and was
    // written as a 1-element tuple by `copy_root`; slot 0 is in bounds.
    let middle = process
        .mailbox()
        .iter()
        .nth(1)
        .expect("the middle message")
        .payload();
    unsafe {
        assert_eq!(signed_value(tuple_element(middle, 0)), 2);
    }
    Ok(())
}

/// A **relocation-exempt** process never enters the re-install path: its collection entries refuse
/// with the typed raise (and `collect_if_needed` is a no-op), so a queued packet is exactly as it
/// was — fragments included.
#[test]
fn an_exempt_process_refuses_the_collection_and_leaves_the_queued_message_untouched() -> Result<()> {
    let mut process = Process::new(pid(1))?;
    let mut arena = Heap::new(64)?;
    let source = arena.tuple(&[make_small(5), make_small(6)])?;
    process
        .mailbox_mut()
        .enqueue(fragmented_message(source, pid(2).to_eterm(), 64)?);
    process.set_relocation_exempt(true);

    assert!(
        !process.collect_if_needed(usize::MAX)?,
        "an exempt process never collects here, so no collection ran"
    );
    match process.collect() {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some("relocation_exempt: collect"),
            "collect refuses with the typed relocation_exempt raise"
        ),
        other => panic!("expected the typed relocation_exempt raise, got {other:?}"),
    }
    match process.ensure_capacity(64) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some("relocation_exempt: ensure_capacity"),
            "ensure_capacity refuses with the typed relocation_exempt raise"
        ),
        other => panic!("expected the typed relocation_exempt raise, got {other:?}"),
    }

    assert_eq!(process.collector().minor_gcs(), 0, "no collection ran");
    let message = process
        .mailbox()
        .peek()
        .expect("the message is still queued");
    assert!(
        message.fragments().is_some(),
        "the refused collection never touched the queued packet"
    );
    assert!(compare::eq(source, message.payload(), &AtomTable::new()));
    Ok(())
}
