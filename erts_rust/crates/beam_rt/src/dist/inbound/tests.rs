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

//! The inbound sink's suite: the **heap-fragment** fallback a receiver-heap refusal now
//! takes (so a peer's oversized message is really delivered), the rule that a *delivery*
//! refusal never retires the link, and the unchanged `NoSuchProcess` answer for a pid the
//! table does not hold.
//!
//! Hermetic — a standalone [`ProcessTable`] and a scratch [`Heap`], no runtime, no socket,
//! no `epmd` (the fixture style of `src/dist/links/tests.rs`).

use super::*;

use beam_alloc::Heap;
use beam_process::{ProcessState, SignalAtoms};
use beam_term::prelude::{eq, make_small, AtomTable, NIL};

/// This test node's name; it is the sink's `local_node` atom.
const NODE_NAME: &str = "inbound-fragment-under-test@host";

/// This test node's creation (the incarnation an addressed external pid must carry to be
/// a process of this node).
const NODE_CREATION: u32 = 0x0BAD_F00D;

/// The payload's list length.
///
/// A cons cell is two words, so this is 1 200 words — comfortably past the default
/// 233-word nursery a freshly spawned process owns, which is what makes the receiver's
/// own heap refuse the copy (and therefore what puts the delivery on the fragment route).
const PAYLOAD_ELEMENTS: i64 = 600;

/// The message envelope's sender word these tests deliver from.
const SENDER: i64 = 7;

/// A hermetic delivery fixture: the atom table the fixture's atoms live in, a standalone
/// process table with one spawned local process, and that node's name atom.
struct Fixture {
    /// The atom table `local_node` (and every fixture atom) lives in.
    atoms: AtomTable,
    /// The process table the sink routes into.
    table: ProcessTable,
    /// One spawned local process — the addressee of these tests.
    pid: ProcessId,
    /// This node's name atom (the sink's `local_node`).
    local_node: Atom,
}

impl Fixture {
    /// A table with one spawned process, for a node named [`NODE_NAME`]/[`NODE_CREATION`].
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        let local_node = atoms.intern(NODE_NAME);
        let signals = SignalAtoms::from_table(&mut atoms);
        let mut table = ProcessTable::new(signals);
        let pid = table.spawn_default().expect("spawn a local process");
        Fixture {
            atoms,
            table,
            pid,
            local_node,
        }
    }

    /// Every `(payload, envelope sender)` pair `pid`'s mailbox holds, in FIFO order.
    fn mailbox(&self, pid: ProcessId) -> Vec<(Eterm, Eterm)> {
        self.table
            .get(pid)
            .expect("the addressed process is live")
            .mailbox()
            .iter()
            .map(|message| (message.payload(), message.from()))
            .collect()
    }
}

/// A `DOP_SEND`-shaped delivery of `message` to `to`, from [`SENDER`].
///
/// A free function rather than a `Fixture` method: the sink holds `&mut fixture.table` for
/// as long as it is alive, so the classified delivery is built before the sink exists.
fn send(to: Eterm, message: Eterm) -> Delivery {
    Delivery::Send {
        from: make_small(SENDER),
        to,
        message,
        token: None,
    }
}

/// A list of [`PAYLOAD_ELEMENTS`] small integers, built in the scratch arena `scratch` —
/// the oversized payload every test below delivers.
fn payload(scratch: &mut Heap) -> Eterm {
    let mut list = NIL;
    for element in (1..=PAYLOAD_ELEMENTS).rev() {
        list = scratch
            .cons(make_small(element), list)
            .expect("the scratch arena holds the whole list");
    }
    list
}

/// A receiver that must not be relocated (its roots are unpublished, so `Process::deliver`
/// returns the copier's raw refusal) still gets the message — through a **heap fragment**,
/// and the pair `(pid, woken)` is recorded so the caller can replay the runtime's two
/// fragment notes (the reachability note for the pid, the wake note only when the delivery
/// really woke it).
///
/// The fixture's receiver is **`Runnable`** (a fresh process) and stays so: `Process::wake`
/// moves only a `Waiting`/`Suspended` receiver, so this delivery does not wake it and the
/// recorded flag is `false`. That is precisely the shape that used to be replayed as a
/// wake — and so enqueued a process nothing should run.
#[test]
fn a_receiver_that_refuses_the_heap_copy_gets_the_message_in_a_heap_fragment() {
    let mut fixture = Fixture::new();
    let pid = fixture.pid;
    let node = fixture.local_node;
    // A relocation-exempt receiver is exactly the shape whose delivery refusal is raw:
    // its live words may be outside the crate root set, so `deliver` neither collects nor
    // grows — it reports the refusal the sink must fall back on.
    fixture
        .table
        .set_relocation_exempt(pid, true)
        .expect("mark the receiver relocation-exempt");
    let mut scratch = Heap::new(4096).expect("a scratch arena for the payload");
    let message = payload(&mut scratch);
    let delivery = send(pid.to_eterm(), message);

    let (outcome, fragments) = {
        let mut sink = LocalDeliverySink::new(&mut fixture.table, node, NODE_CREATION);
        let outcome = sink
            .deliver(&delivery)
            .expect("a receiver-heap refusal is not an internal error");
        (outcome, sink.take_fragment_deliveries())
    };

    assert_eq!(
        outcome,
        DeliveryOutcome::Delivered,
        "the fragment fallback delivers the message instead of dropping it"
    );
    assert_eq!(
        fragments,
        vec![(pid, false)],
        "exactly one fragment delivery is recorded, for the addressed receiver, and it did \
         NOT wake it: the receiver was already `Runnable` (`Process::wake` moves only a \
         `Waiting`/`Suspended` one), so replaying a wake note for it would enqueue a process \
         nothing is waiting to run"
    );
    assert_eq!(
        fixture
            .table
            .get(pid)
            .expect("the receiver is live")
            .state(),
        ProcessState::Runnable,
        "the delivery left the receiver's state exactly as it found it"
    );
    let mailbox = fixture.mailbox(pid);
    assert_eq!(
        mailbox.len(),
        1,
        "the receiver's mailbox holds exactly the delivered message: {mailbox:?}"
    );
    assert_eq!(
        mailbox[0].1,
        make_small(SENDER),
        "the envelope carries the sender word"
    );
    assert!(
        eq(mailbox[0].0, message, &fixture.atoms),
        "the delivered payload compares equal to the original, element for element"
    );
}

/// A delivery the receiver's heap refuses is **never** a link-level failure: the sink
/// answers `Ok`, so a caller that retires a link on `Err` keeps this peer's link up.
#[test]
fn a_delivery_refusal_is_never_a_link_failure() {
    let mut fixture = Fixture::new();
    let pid = fixture.pid;
    let node = fixture.local_node;
    fixture
        .table
        .set_relocation_exempt(pid, true)
        .expect("mark the receiver relocation-exempt");
    let mut scratch = Heap::new(4096).expect("a scratch arena for the payload");
    let message = payload(&mut scratch);
    let delivery = send(pid.to_eterm(), message);

    let mut sink = LocalDeliverySink::new(&mut fixture.table, node, NODE_CREATION);
    let result = sink.deliver(&delivery);
    assert!(
        result.is_ok(),
        "a receiver-heap refusal must not surface as an Err (that would retire the link): \
         {result:?}"
    );
    assert_eq!(
        result.expect("checked just above"),
        DeliveryOutcome::Delivered,
        "and it is a real delivery, not the honest drop"
    );
}

/// A delivery addressed at a pid the table does not hold is still `NoSuchProcess`, and
/// records no fragment delivery.
#[test]
fn an_unknown_pid_is_still_no_such_process() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    // A pid naming a slot this table never issued (the number is far past the one slot the
    // fixture spawned).
    let ghost = ProcessId::new(4242, 0);
    assert!(!fixture.table.contains(ghost));
    let mut scratch = Heap::new(4096).expect("a scratch arena for the payload");
    let message = payload(&mut scratch);
    let delivery = send(ghost.to_eterm(), message);

    let (outcome, fragments) = {
        let mut sink = LocalDeliverySink::new(&mut fixture.table, node, NODE_CREATION);
        let outcome = sink
            .deliver(&delivery)
            .expect("an unknown pid is an outcome, not an error");
        (outcome, sink.take_fragment_deliveries())
    };

    assert_eq!(outcome, DeliveryOutcome::NoSuchProcess);
    assert!(
        fragments.is_empty(),
        "a pid with no process records no fragment delivery"
    );
}

/// A fragment delivery to a receiver that really **was** parked records `woken == true` —
/// the leg that must still reach the runtime's wake list, and so still be enqueued.
///
/// The receiver is put in the state a blocked `receive` leaves it in (`Waiting`) and stays
/// relocation-exempt (so the heap copy is refused and the fragment route is taken). The
/// delivery's own `Process::wake` is what moves it back to `Runnable`, which is exactly what
/// the recorded flag has to report — the delivery is the reason it can run again.
#[test]
fn a_fragment_delivery_that_wakes_a_parked_receiver_records_the_wake() {
    let mut fixture = Fixture::new();
    let pid = fixture.pid;
    let node = fixture.local_node;
    fixture
        .table
        .set_relocation_exempt(pid, true)
        .expect("mark the receiver relocation-exempt");
    {
        let receiver = fixture.table.get_mut(pid).expect("the receiver is live");
        receiver.set_state(ProcessState::Waiting);
    }
    let mut scratch = Heap::new(4096).expect("a scratch arena for the payload");
    let message = payload(&mut scratch);
    let delivery = send(pid.to_eterm(), message);

    let (outcome, fragments) = {
        let mut sink = LocalDeliverySink::new(&mut fixture.table, node, NODE_CREATION);
        let outcome = sink
            .deliver(&delivery)
            .expect("a receiver-heap refusal is not an internal error");
        (outcome, sink.take_fragment_deliveries())
    };

    assert_eq!(outcome, DeliveryOutcome::Delivered);
    assert_eq!(
        fragments,
        vec![(pid, true)],
        "the parked receiver really was woken, so the wake note must be replayed for it"
    );
    assert_eq!(
        fixture
            .table
            .get(pid)
            .expect("the receiver is live")
            .state(),
        ProcessState::Runnable,
        "a woken receiver is runnable again"
    );
    let mailbox = fixture.mailbox(pid);
    assert_eq!(
        mailbox.len(),
        1,
        "and the fragment-resident message is in its mailbox: {mailbox:?}"
    );
}
