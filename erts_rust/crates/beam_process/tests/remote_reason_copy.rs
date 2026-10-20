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

//! The **inbound (wire) reason copy**: an exit/DOWN reason a host decoded off the wire lives on a
//! **transient** heap — `beam_rt` decodes a whole drain onto one `dispatch_heap` and calls
//! `reset()` on it before the next drain (`dist/node.rs::dispatch_inbound`) — so a queued
//! `{'EXIT', …}`/`{'DOWN', …}` may never hold that word. The two remote builders
//! ([`ProcessTable::apply_remote_exit_signal`]'s trap arm and
//! [`ProcessTable::remote_monitor_down`]) therefore **materialise the reason on the recipient's own
//! heap** (or, when that heap refuses the copy, inside the message's [`beam_message`] heap
//! fragment) before embedding it, exactly as the local queued-signal path already did through
//! `copy_reason_onto`.
//!
//! Every case here builds a **boxed** reason on a separate arena `Heap`, delivers it through the
//! public receipt API, then **recycles the arena the way a host's next drain does** — `reset()`
//! plus a same-shaped decode that overwrites the very words the reason occupied — and only then
//! reads the queued message. So a stale word cannot pass:
//!
//! * the queued reason word must not alias the arena word (checked **first**, so a stale word
//!   never gets dereferenced), and
//! * the structural walk (a 2-tuple, an atom tag, a list of the exact small integers) reads only
//!   the recipient's own heap / the message's fragment.
//!
//! An **immediate** reason is position-independent, which is why the (immediate-only) oracle
//! cross-checks in `tests/remote_inbound.rs` passed before the copy existed; those file's cases
//! stay as they are.
//!
//! The cases:
//!
//! 1. [`a_trapping_link_exit_copies_the_wire_reason_onto_the_receivers_heap`] — an inbound
//!    `DOP_EXIT`'s boxed reason to a trapping receiver;
//! 2. [`a_remote_monitor_down_copies_the_wire_reason_onto_the_watchers_heap`] — an inbound
//!    `DOP_MONITOR_P_EXIT`'s boxed reason to the local watcher;
//! 3. [`an_exempt_receiver_gets_the_boxed_wire_reason_in_the_messages_heap_fragment`] — the same
//!    exit with a reason larger than a **relocation-exempt** receiver's whole nursery: the copy is
//!    refused, the message goes into a heap fragment, and the nursery is neither collected nor
//!    moved;
//! 4. [`a_gate_declined_watcher_gets_the_boxed_wire_reason_in_the_messages_heap_fragment`] — the
//!    DOWN half of the same decline, through the host's `CollectionGate`.
//!
//! Determinism: no sleep, no wall-clock bound, no oracle. The "was it copied" question is answered
//! by an address comparison and a structural walk performed **after** the arena is gone; the
//! "did nothing move" claim by the receiver's own nursery observables.

use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_gc::HeapPolicy;
use beam_message::Message;
use beam_process::prelude::*;
use beam_term::list::{car, cdr};
use beam_term::prelude::{
    external_pid_size_words, is_list, is_small, make_atom, make_small, signed_value, tuple_element,
    tuple_size, write_external_pid, AtomTable, Eterm, NIL,
};

/// The words the fragment cases' reason occupies: far more than the tiny nursery below, so the
/// *reason copy* onto the receiver's own heap is the request that is refused.
const OVERSIZED_INT_COUNT: i64 = 10;

/// The receiver's nursery in words for the fragment cases — smaller than one boxed reason, so the
/// copy is refused by capacity (never by timing).
const SMALL_NURSERY_WORDS: usize = 16;

/// A table, the atom table its [`SignalAtoms`] were interned from, and the sysname atom term
/// standing in for the remote node.
fn fixture() -> (ProcessTable, AtomTable, Eterm) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let node = make_atom(names.intern("remote@host"));
    // The reason tags and the arena's clobber tag, interned in the very index space the table's
    // [`SignalAtoms`] use (an atom is a table-local index, so a foreign table would mis-name them).
    for name in ["boom", "shutdown", "clobber"] {
        names.intern(name);
    }
    (ProcessTable::new(atoms), names, node)
}

/// The term for a name the fixture interned.
fn named(names: &AtomTable, name: &str) -> Eterm {
    make_atom(names.lookup(name).expect("the fixture interned it"))
}

/// The heap a decoded external pid is built on. Only the *term* is a heap object — a
/// [`RemotePid`] decomposes it into plain values — so this heap never has to outlive the call.
fn wire_pid_heap() -> Heap {
    Heap::new(32).expect("the wire heap allocates")
}

/// Builds an external pid term on `heap` (the frozen 4-word `[header, node, creation, data]`
/// thing).
fn remote_pid(heap: &mut Heap, node: Eterm, number: u32, serial: u32, creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned, writable words.
    let p = unsafe {
        heap.alloc_words(external_pid_size_words())
            .expect("the heap has room")
    };
    // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all of
    // them before the pointer escapes; `node` is a live atom term.
    unsafe { write_external_pid(p, node, number, serial, creation) }
}

/// A decode arena: the transient heap a host decodes a whole distribution drain onto.
fn arena() -> Heap {
    Heap::new(64).expect("the arena allocates")
}

/// Builds `{Tag, [start, start+1, …, start+len-1]}` on `arena` — a **boxed** exit/monitor reason
/// whose words all live on the arena, exactly what a decoded `DOP_*` reason is.
fn boxed_reason(arena: &mut Heap, tag: Eterm, start: i64, len: i64) -> Eterm {
    let mut list = NIL;
    for i in (0..len).rev() {
        list = arena
            .cons(make_small(start + i), list)
            .expect("the arena has room");
    }
    arena.tuple(&[tag, list]).expect("the arena has room")
}

/// Recycles the arena the way a host's **next drain** does — `reset()` the bump pointer, then
/// decode again — writing a `{clobber, [1000, …]}` term over the words the delivered reason
/// occupied. It is built **longer** than every reason this file delivers, so every word of the
/// delivered reason is really rewritten: a queued word that still pointed into the arena would
/// read `start = 1000`, a cons cell where the reason's tuple header was, or both.
fn recycle(arena: &mut Heap, names: &AtomTable) {
    arena.reset();
    let junk = boxed_reason(arena, named(names, "clobber"), 1000, OVERSIZED_INT_COUNT + 4);
    // The rewrite really happened (asserted through the arena's own live words).
    assert_boxed_reason(junk, named(names, "clobber"), 1000, OVERSIZED_INT_COUNT + 4);
}

/// The single message queued for `pid`.
fn sole_message(table: &ProcessTable, pid: ProcessId) -> &Message {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued")
}

/// Asserts `reason` really is `{tag, [start, …, start+len-1]}` — read **only** through the words
/// the recipient owns (its heap, or the message's fragment).
fn assert_boxed_reason(reason: Eterm, tag: Eterm, start: i64, len: i64) {
    // SAFETY: `reason` is the live 2-tuple an assertion below proved is queued on a heap this test
    // ownes through `table` (or inside the message's fragment); the two element slots are in
    // bounds, and every cons cell of the element-1 list is a live `[CAR, CDR]` pair until `NIL`.
    unsafe {
        assert_eq!(tuple_size(reason), 2, "the reason is a 2-tuple");
        assert_eq!(tuple_element(reason, 0), tag, "the tuple's tag survived");
        let mut cell = tuple_element(reason, 1);
        for i in 0..len {
            assert!(is_list(cell), "element {i} of the reason list is a cons cell");
            let head = car(cell);
            assert!(is_small(head), "element {i} of the reason list is a small integer");
            assert_eq!(
                signed_value(head),
                start + i,
                "element {i} of the reason list survived the copy"
            );
            cell = cdr(cell);
        }
        assert_eq!(cell, NIL, "the reason list has exactly {len} elements");
    }
}

/// The receiver's nursery observables: its minor-GC count, the young space's base pointer and its
/// used words. A collection or a growth moves the first two; a copy moves the third.
fn nursery(table: &ProcessTable, pid: ProcessId) -> (u32, *mut Eterm, usize) {
    let process = table.get(pid).expect("the receiver is live");
    (
        process.collector().minor_gcs(),
        process.collector().young().base(),
        process.heap_used_words(),
    )
}

/// The only queued message of `pid`, with the evidence that it is **fragment**-resident (the
/// payload root lives inside the fragment, not on the receiver's heap).
fn fragment_message(table: &ProcessTable, pid: ProcessId) -> &Message {
    let message = sole_message(table, pid);
    let fragments = message
        .fragments()
        .expect("the declined receiver got a heap-fragment message");
    assert_eq!(fragments.len(), 1, "one fragment carries the whole payload");
    assert!(
        fragments
            .iter()
            .next()
            .expect("one fragment")
            .contains(message.payload()),
        "the payload root lives inside the fragment, not on the receiver's heap"
    );
    message
}

// --- DOP_EXIT / DOP_PAYLOAD_EXIT --------------------------------------------

#[test]
fn a_trapping_link_exit_copies_the_wire_reason_onto_the_receivers_heap() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_pid_heap();
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("receiver").set_trap_exit(true);
    let ext = remote_pid(&mut wire, node, 21, 22, 23);
    let remote = RemotePid::from_eterm(ext)?;
    table.record_remote_link(receiver, remote)?;

    // The reason is decoded onto the host's per-drain arena.
    let mut arena = arena();
    let reason = boxed_reason(&mut arena, named(&names, "boom"), 1, 3);
    let reason_word = reason.raw();
    table.apply_remote_exit_signal(receiver, remote, reason, false)?;

    // The drain ends: the arena is rewound and its words reused for the next decode.
    recycle(&mut arena, &names);
    drop(arena);

    let message = sole_message(&table, receiver);
    assert_eq!(
        message.from(),
        node,
        "the exit message `from` slot is the remote node atom (an immediate)"
    );
    let payload = message.payload();
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple on the receiver's heap.
    let copied = unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1))?,
            remote,
            "the body's `From` is still the remote external pid"
        );
        tuple_element(payload, 2)
    };
    assert_ne!(
        copied.raw(),
        reason_word,
        "the queued reason is a COPY — never the arena word the host is about to recycle"
    );
    assert_boxed_reason(copied, named(&names, "boom"), 1, 3);
    Ok(())
}

// --- DOP_MONITOR_P_EXIT -----------------------------------------------------

#[test]
fn a_remote_monitor_down_copies_the_wire_reason_onto_the_watchers_heap() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_pid_heap();
    let watcher = table.spawn_default()?;
    let ext = remote_pid(&mut wire, node, 31, 32, 33);
    let remote = RemotePid::from_eterm(ext)?;
    // The outgoing record the inbound DOWN resolves (and removes).
    let reference = table.monitor_term(watcher, ext, MonitorKind::Process)?;

    let mut arena = arena();
    let reason = boxed_reason(&mut arena, named(&names, "shutdown"), 1, 3);
    let reason_word = reason.raw();
    table.remote_monitor_down(watcher, remote, reference, reason)?;

    recycle(&mut arena, &names);
    drop(arena);

    assert!(
        table.remote_monitors(watcher).is_empty(),
        "the outgoing RemoteMonitor was resolved by the DOWN"
    );
    let message = sole_message(&table, watcher);
    assert_eq!(message.from(), node, "the DOWN message `from` is the node atom");
    let payload = message.payload();
    // SAFETY: `payload` is the live `{'DOWN', Ref, Kind, Target, Reason}` tuple on the watcher's
    // heap; the five element slots are in bounds.
    let copied = unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
        assert_eq!(tuple_element(payload, 2), named(&names, "process"));
        assert_eq!(RemotePid::from_eterm(tuple_element(payload, 3))?, remote);
        tuple_element(payload, 4)
    };
    assert_ne!(
        copied.raw(),
        reason_word,
        "the queued DOWN reason is a COPY — never the arena word"
    );
    assert_boxed_reason(copied, named(&names, "shutdown"), 1, 3);
    Ok(())
}

// --- the heap-fragment route of the same copy -------------------------------

#[test]
fn an_exempt_receiver_gets_the_boxed_wire_reason_in_the_messages_heap_fragment() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_pid_heap();
    let receiver = table.spawn(HeapPolicy {
        initial_young_words: SMALL_NURSERY_WORDS,
        ..HeapPolicy::default()
    })?;
    {
        let process = table.get_mut(receiver).expect("receiver");
        process.set_trap_exit(true);
        // The host holds live terms of this heap outside the crate root set.
        process.set_relocation_exempt(true);
    }
    let ext = remote_pid(&mut wire, node, 41, 42, 43);
    let remote = RemotePid::from_eterm(ext)?;
    table.record_remote_link(receiver, remote)?;
    let before = nursery(&table, receiver);

    let mut arena = arena();
    let reason = boxed_reason(&mut arena, named(&names, "boom"), 1, OVERSIZED_INT_COUNT);
    let reason_word = reason.raw();
    table.apply_remote_exit_signal(receiver, remote, reason, false)?;

    assert_eq!(
        nursery(&table, receiver),
        before,
        "the exempt receiver is neither collected, grown nor relocated for the signal"
    );

    recycle(&mut arena, &names);
    drop(arena);

    let message = fragment_message(&table, receiver);
    assert_eq!(message.from(), node, "the `from` slot stays the node atom");
    let payload = message.payload();
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple the fragment holds.
    let copied = unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(RemotePid::from_eterm(tuple_element(payload, 1))?, remote);
        tuple_element(payload, 2)
    };
    assert_ne!(copied.raw(), reason_word, "the fragment holds a COPY of the reason");
    assert_boxed_reason(copied, named(&names, "boom"), 1, OVERSIZED_INT_COUNT);
    Ok(())
}

#[test]
fn a_gate_declined_watcher_gets_the_boxed_wire_reason_in_the_messages_heap_fragment() -> Result<()> {
    /// A host gate that declines every relocation.
    struct Declines {
        asked: Rc<std::cell::Cell<usize>>,
    }

    impl CollectionGate for Declines {
        fn declines_relocation(&mut self, _receiver: &Process) -> bool {
            self.asked.set(self.asked.get() + 1);
            true
        }
    }

    let (mut table, names, node) = fixture();
    let mut wire = wire_pid_heap();
    let watcher = table.spawn(HeapPolicy {
        initial_young_words: SMALL_NURSERY_WORDS,
        ..HeapPolicy::default()
    })?;
    let ext = remote_pid(&mut wire, node, 51, 52, 53);
    let remote = RemotePid::from_eterm(ext)?;
    let reference = table.monitor_term(watcher, ext, MonitorKind::Process)?;
    let asked = Rc::new(std::cell::Cell::new(0));
    table.set_collection_gate(Box::new(Declines {
        asked: Rc::clone(&asked),
    }));
    let before = nursery(&table, watcher);

    let mut arena = arena();
    let reason = boxed_reason(&mut arena, named(&names, "shutdown"), 1, OVERSIZED_INT_COUNT);
    let reason_word = reason.raw();
    table.remote_monitor_down(watcher, remote, reference, reason)?;

    assert!(
        asked.get() > 0,
        "the host gate really was consulted for the watcher"
    );
    assert_eq!(
        nursery(&table, watcher),
        before,
        "a gate-declined watcher is neither collected, grown nor relocated"
    );

    recycle(&mut arena, &names);
    drop(arena);

    let message = fragment_message(&table, watcher);
    assert_eq!(message.from(), node, "the `from` slot stays the node atom");
    let payload = message.payload();
    // SAFETY: `payload` is the live `{'DOWN', Ref, Kind, Target, Reason}` tuple the fragment holds.
    let copied = unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
        assert_eq!(tuple_element(payload, 2), named(&names, "process"));
        assert_eq!(RemotePid::from_eterm(tuple_element(payload, 3))?, remote);
        tuple_element(payload, 4)
    };
    assert_ne!(copied.raw(), reason_word, "the fragment holds a COPY of the reason");
    assert_boxed_reason(copied, named(&names, "shutdown"), 1, OVERSIZED_INT_COUNT);
    Ok(())
}
