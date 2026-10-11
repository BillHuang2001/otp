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

//! Integration tests for the message envelope's **GC-rooting (relocation) surface**.
//!
//! The owner of a queued message (`beam_process::Process::run_rooted_collection`) gathers the
//! message's payload as a GC root and writes the relocated word back. The envelope's other two
//! **term** fields — `from` and `token` — need the same treatment, so this crate exposes the
//! mutating setters [`set_from`](beam_message::Message::set_from)/[`set_token`](beam_message::Message::set_token)
//! plus the builder [`with_from`](beam_message::Message::with_from). The tests below pin that:
//!
//! * the two setters replace **exactly** those two words and leave the payload, the off-heap
//!   handle list, the refc-binary list, the node list and the fragment chain untouched,
//! * `with_from` re-targets the sender and keeps the rest,
//! * a host-style gather/relocate/scatter round-trip relocates `payload`/`from`/`token`,
//! * the **enqueue tripwire** (`Queue::enqueue`, debug-only) fires loudly when a host enqueues an
//!   un-relocated, non-immediate `from`/`token` — or a bare non-value header word,
//! * a well-formed envelope (immediate `from`/`token`) enqueues and reads back unchanged.

use beam_alloc::{Heap, RefcBinary};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct_simple, Fragments, HeapFragment, Message, NodeEntry, Queue};
use beam_term::prelude::*;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first, synchronously (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the heap");
    }
    acc
}

/// A 200-byte payload whose bytes are position-dependent, so a partial or shifted copy is visible.
fn bytes_200() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// A non-trivial envelope: an immediate `from`/`token`, one MSO handle, one refc binary, one node
/// reference and a one-fragment chain holding a genuine fragment-resident payload.
///
/// The enclosing message is returned together with the fragment-resident payload, so a test can
/// assert the payload still lives in the fragment chain after the setters ran.
fn non_trivial_envelope() -> (Message, Eterm) {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    let bytes = bytes_200();
    let binary = RefcBinary::new(&bytes).expect("the payload allocates");

    // One MSO handle, taken out of its table (the builder takes an owned `Vec<MsoHandle>`).
    let mut off = OffHeapTable::new();
    off.push(Mso::new(0xABCD_u32, 8));
    let handles = off.into_handles();
    assert_eq!(handles.len(), 1);

    // A genuine fragment-resident payload: copy a list into the fragment's region.
    let mut src = Heap::new(64).expect("source heap");
    let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);
    let src_off = OffHeapTable::new();
    let mut fragment = HeapFragment::new(16).expect("the fragment allocates");
    let payload = fragment.copy_root(term, &src_off).expect("the copy fits");
    let mut fragments = Fragments::new();
    fragments.push(fragment);

    let message = Message::new(payload, make_small(7))
        .with_token(make_small(9))
        .with_off_heap(handles)
        .with_binaries(vec![binary])
        .with_nodes(vec![NodeEntry::new(node, 2)])
        .with_fragments(fragments);

    (message, payload)
}

#[test]
fn set_from_and_set_token_replace_exactly_the_two_term_words() {
    let (mut message, payload) = non_trivial_envelope();

    let new_from = make_small(4242);
    let new_token = make_small(99);
    message.set_from(new_from);
    message.set_token(new_token);

    assert_eq!(message.from(), new_from, "the sender is the relocated word");
    assert_eq!(message.token(), new_token, "the token is the relocated word");
    assert_eq!(message.payload().raw(), payload.raw(), "the payload word is untouched");

    // Every non-term list and the fragment chain survive the two setter calls unchanged.
    assert_eq!(message.off_heap().len(), 1, "the off-heap handle survives");
    assert_eq!(
        message.off_heap()[0].downcast_ref::<u32>(),
        Some(&0xABCD_u32),
        "the same MSO object is still referenced"
    );

    assert_eq!(message.binaries().len(), 1, "the refc binary survives");
    assert_eq!(message.binaries()[0].as_bytes(), &bytes_200()[..]);
    assert_eq!(message.logical_binary_bytes(), 200);

    assert_eq!(message.nodes().len(), 1, "the node reference survives");
    assert_eq!(message.nodes()[0].creation(), 2);

    let fragments = message.fragments().expect("the fragment chain survives");
    assert_eq!(fragments.len(), 1);
    let fragment = fragments.iter().next().expect("one fragment");
    assert!(
        fragment.contains(payload),
        "the payload still lives in the fragment region"
    );
}

#[test]
fn with_from_re_targets_the_sender_and_keeps_the_rest() {
    let payload = make_small(1);
    let message = Message::new(payload, make_small(7)).with_token(make_small(9));

    let retargeted = message.with_from(make_small(42));

    assert_eq!(retargeted.from(), make_small(42), "the sender is re-targeted");
    assert_eq!(retargeted.payload(), payload, "the payload survives");
    assert_eq!(retargeted.token(), make_small(9), "the token survives");

    // The non-term lists are carried too.
    let (envelope, fragment_payload) = non_trivial_envelope();
    let rebuilt = envelope.with_from(make_small(42));
    assert_eq!(rebuilt.from(), make_small(42));
    assert_eq!(rebuilt.off_heap().len(), 1);
    assert_eq!(rebuilt.binaries().len(), 1);
    assert_eq!(rebuilt.nodes().len(), 1);
    assert!(
        rebuilt
            .fragments()
            .expect("the fragment chain survives")
            .iter()
            .next()
            .expect("one fragment")
            .contains(fragment_payload)
    );
}

#[test]
fn host_style_relocation_round_trip_relocates_payload_from_and_token() {
    // A heap-resident message, exactly as the host sees it before a collection.
    let mut src = Heap::new(64).expect("source heap");
    let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);
    let mut message = Message::new(term, make_small(7)).with_token(make_small(9));

    // The host's gather step: the payload *and* the two term words are roots.
    let (payload_root, from_root, token_root) = (message.payload(), message.from(), message.token());
    assert!(is_immediate(from_root) && is_immediate(token_root));

    // The collection: the payload graph is relocated into a fresh nursery.
    let mut dst = Heap::new(64).expect("destination heap");
    let relocated_payload = copy_struct_simple(payload_root, &mut dst).expect("the copy succeeds");
    assert!(
        (relocated_payload.raw() as usize) != (payload_root.raw() as usize),
        "the payload moved to the fresh heap"
    );

    // The host's scatter step: write every gathered root back, the two term words through the new
    // setters.
    message = message.with_payload(relocated_payload);
    message.set_from(from_root);
    message.set_token(token_root);

    assert_eq!(message.payload().raw(), relocated_payload.raw());
    assert_eq!(message.from(), make_small(7), "from reads back the gathered word");
    assert_eq!(message.token(), make_small(9), "token reads back the gathered word");
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Message::from is not an immediate")]
fn enqueueing_a_heap_from_trips_the_debug_assert() {
    let mut heap = Heap::new(8).expect("heap");
    let heap_from = build_list(&mut heap, &[make_small(1)]);
    assert!(!is_immediate(heap_from), "a cons cell is not an immediate");

    let mut queue = Queue::new();
    queue.enqueue(Message::new(make_small(1), heap_from));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Message::token is not an immediate")]
fn enqueueing_a_heap_token_trips_the_debug_assert() {
    let mut heap = Heap::new(8).expect("heap");
    let heap_token = build_list(&mut heap, &[make_small(1)]);
    assert!(!is_immediate(heap_token), "a cons cell is not an immediate");

    let mut queue = Queue::new();
    queue.enqueue(Message::new(make_small(1), make_small(7)).with_token(heap_token));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Message::from is a bare non-value header word")]
fn enqueueing_a_non_value_from_trips_the_debug_assert() {
    let mut queue = Queue::new();
    queue.enqueue(Message::new(make_small(1), THE_NON_VALUE));
}

#[test]
fn an_immediate_envelope_enqueues_and_reads_back_unchanged() {
    let mut queue = Queue::new();

    // A message as the producers build it: immediate `from`/`token`.
    queue.enqueue(Message::new(make_small(1), make_small(7)).with_token(make_small(9)));
    // And one whose two term words were just written by the relocation setters.
    let mut relocated = Message::new(make_small(2), make_small(0));
    relocated.set_from(make_small(8));
    relocated.set_token(NIL);
    queue.enqueue(relocated);

    assert_eq!(queue.len(), 2);
    let head = queue.peek().expect("one queued message");
    assert_eq!(head.from(), make_small(7));
    assert_eq!(head.token(), make_small(9));

    let second = queue.iter().nth(1).expect("two queued messages");
    assert_eq!(second.payload(), make_small(2));
    assert_eq!(second.from(), make_small(8));
    assert_eq!(second.token(), NIL, "the absent-token NIL is an immediate");
}
