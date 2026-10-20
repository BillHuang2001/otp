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

#![deny(unsafe_code)]

//! Remote `erlang:exit/2` (contract A.5) end to end: the signal the process layer emits,
//! the real wire codec, and the receiver's route-free receipt.
//!
//! Two **real** process tables are placed on either side of a real
//! [`beam_dist::ConnectionDriver`] pair:
//!
//! * table **A** is the *dialer* (`client@host`, creation [`CLIENT_CREATION`]) — the
//!   signaller. Its [`ProcessTable`] holds one process and has a **recording**
//!   [`DistSignalRouter`] installed, exactly like `tests/dist_delivery.rs`'s
//!   `RecordingRouter`, so the signal `erlang:exit/2` emits can be inspected before it is
//!   serialised.
//! * table **B** is the *listener* (`server@host`, creation [`SERVER_CREATION`]) — the
//!   recipient. Its [`ProcessTable`] holds the targets, the server driver carries node B's
//!   [`LocalIdentity`] exactly as every drained link of the runtime does (so a wire pid
//!   naming this node arrives **internal**, the C's `dec_is_this_node`, `external.c:3123`),
//!   and the decoded control message is fed to it through the same [`LocalDeliverySink`] the
//!   runtime installs, i.e. through `ProcessTable::apply_remote_exit_signal` (the C's
//!   `DOP_EXIT2` receipt arm, `dist.c:2824`, which consults no link — unlike `DOP_EXIT`'s
//!   link-exit family).
//!
//! The wire is the real one: the client driver encodes `{DOP_EXIT2, From, To, Reason}` and
//! the server driver decodes and classifies it into `Delivery::Exit2`. The bytes are
//! shuttled by hand ([`relay`], the harness of `tests/dist_delivery.rs` / the peer crate's
//! `tests/delivery.rs`), so this suite is **hermetic**: no socket, no `epmd`, no system
//! `erl`.
//!
//! # What the three oracle cases pin
//!
//! `erlang:exit(Pid, Reason)` on a **remote** pid is a plain, untrappable-`kill` exit signal
//! (`bif.c:1804-1838` → `erts_dsig_send_exit2` → `true`), and the receiver applies the C's
//! exit-policy table (`erl_proc_sig_queue.c:4740-4770`):
//!
//! | case | emitted | received |
//! |---|---|---|
//! | `kill` to a non-trapping target | `DOP_EXIT2` | the target dies, reason rewritten to `killed` |
//! | `normal` to a non-trapping target | `DOP_EXIT2` | the target **survives**, no message |
//! | any reason to a `trap_exit` target | `DOP_EXIT2` | the target survives with `{'EXIT', From, Reason}` |
//! | `kill` to a `trap_exit` target | `DOP_EXIT2` | the target still dies (`kill` is untrappable) |
//!
//! # The emitted side's answer
//!
//! The BIF's own answer — the oracle's `true` for a remote target — is
//! `src/native/process/exit.rs::exit_remote`'s, pinned by that module's tests; reaching it
//! from this harness would need a whole `Runtime` on both ends. What this suite pins instead
//! is the layer underneath it: `send_exit_term` returns `Ok(())` and the routing seam it
//! feeds receives **exactly one** `DistSignal::Exit2` carrying the caller's pid, the
//! recipient's remote pid and the reason. `send_exit_term` is the very entry point
//! `exit_remote` calls, so the two together cover `exit(Pid, Reason) -> true` and what it
//! puts on the wire.
//!
//! # Namespaces
//!
//! An atom's *name* is what crosses the wire, never its index: A encodes `kill` from its own
//! atom table and B re-interns the name into its own, which may land on a different index.
//! Every expectation below is therefore materialised in the namespace that will compare it
//! ([`Reason::materialise`] is called once per side, and `RemotePid` fields — not raw terms —
//! carry the decoded pid's identity so the assertions are name-based).

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_codec::term::make_external_pid;
use beam_common::Result as BeamResult;
use beam_dist::flags::Dop;
use beam_dist::{ConnectionDriver, Delivery, DeliveryOutcome, DriverState, InboundSink,
    LocalIdentity};
use beam_process::{
    DistSignal, DistSignalRouter, ProcessExit, ProcessExitObserver, ProcessId, ProcessTable,
    RemotePid, SignalAtoms,
};
use beam_rt::dist::LocalDeliverySink;
use beam_term::prelude::{atom_val, eq, make_atom, make_small, Atom, Eterm};
use beam_term::AtomTable;

/// The cookie both ends authenticate with.
const COOKIE: &[u8] = b"s3cr3t-cookie";
/// Node A (the dialer / signaller) and its incarnation — the values
/// [`ConnectionDriver::client`] handshakes with, so the `From` pid names a real peer.
const CLIENT_NODE: &str = "client@host";
/// Node A's incarnation.
const CLIENT_CREATION: u32 = 7;
/// Node B (the listener / recipient) and its incarnation.
const SERVER_NODE: &str = "server@host";
/// Node B's incarnation.
const SERVER_CREATION: u32 = 0xdead_beef;
/// A scratch arena big enough for every term these tests build.
const HEAP_WORDS: usize = 1 << 16;

// ---------------------------------------------------------------------------
// The hermetic two-driver harness (mirrors `tests/dist_delivery.rs`, which
// mirrors `beam_dist/tests/delivery.rs`; an integration-test binary is its own
// crate, so the helpers cannot be shared across the two files)
// ---------------------------------------------------------------------------

/// Drives a client and a server driver against each other until both are established.
fn establish() -> (ConnectionDriver, ConnectionDriver) {
    let mut client = ConnectionDriver::client(CLIENT_NODE.as_bytes(), COOKIE, CLIENT_CREATION);
    let mut server = ConnectionDriver::server(SERVER_NODE.as_bytes(), COOKIE, SERVER_CREATION);
    for _ in 0..100_000 {
        let client_out = client.take_output();
        let server_out = server.take_output();
        if !client_out.is_empty() {
            server.feed(&client_out);
        }
        if !server_out.is_empty() {
            client.feed(&server_out);
        }
        client.step().expect("client step");
        server.step().expect("server step");
        if client.state() == DriverState::Established && server.state() == DriverState::Established {
            return (client, server);
        }
        if client.state() == DriverState::Failed || server.state() == DriverState::Failed {
            panic!("driver failed: client={:?}", client.state());
        }
    }
    panic!("handshake did not complete");
}

/// Shuttles `from`'s queued output into `to` and makes one `step` of progress.
fn relay(from: &mut ConnectionDriver, to: &mut ConnectionDriver) {
    let out = from.take_output();
    assert!(!out.is_empty(), "the sender produced no bytes");
    to.feed(&out);
    to.step().expect("receiver step");
}

// ---------------------------------------------------------------------------
// The two nodes
// ---------------------------------------------------------------------------

/// One distribution node in this harness: a real [`ProcessTable`], the atom table its terms
/// live in, its own and its peer's sysname atoms, its incarnation, and one spawned process.
struct Node {
    /// The process table the signals enter/leave through.
    table: ProcessTable,
    /// The atom table every term this node builds is spelled in.
    atoms: AtomTable,
    /// This node's own sysname atom.
    name: Atom,
    /// The peer node's sysname atom (interned here, so a pid naming the peer is spellable).
    peer: Atom,
    /// This node's incarnation — what an addressed external pid must carry to be its process.
    creation: u32,
    /// One spawned local process: node A's signaller, and a spare target on node B.
    pid: ProcessId,
}

impl Node {
    /// A fresh, node-only fixture named `own` (peer `peer`) with a chosen incarnation.
    fn new(own: &str, peer: &str, creation: u32) -> Node {
        let mut atoms = AtomTable::new();
        let name = atoms.intern(own);
        let peer = atoms.intern(peer);
        // `SignalAtoms::from_table` interns `EXIT`/`normal`/`kill`/`killed`/… into this very
        // table, so the exit policy classifies the reasons these tests use by *index* as well
        // as by name (one table, one namespace — see the module docs).
        let signals = SignalAtoms::from_table(&mut atoms);
        let mut table = ProcessTable::new(signals);
        let pid = table.spawn_default().expect("spawn a local process");
        Node {
            table,
            atoms,
            name,
            peer,
            creation,
            pid,
        }
    }

    /// Spawns one more local process on this node.
    fn spawn(&mut self) -> ProcessId {
        self.table.spawn_default().expect("spawn a local process")
    }

    /// The wire image of one of **this** node's processes: an external pid carrying this
    /// node's name and incarnation — the shape a peer addresses a process of this node as
    /// (`beam_dist`'s decoder turns it back into the internal pid, see [`Wire::new`]).
    fn pid_term(&self, heap: &mut Heap, pid: ProcessId) -> Eterm {
        make_external_pid(
            heap,
            make_atom(self.name),
            pid.number() as u32,
            pid.serial() as u32,
            self.creation,
        )
        .expect("materialize the external pid thing")
    }

    /// The wire image of one of the **peer's** processes, spelled in this node's namespace —
    /// what a signaller addresses as `To`.
    fn peer_pid_term(&self, heap: &mut Heap, pid: ProcessId, peer_creation: u32) -> Eterm {
        make_external_pid(
            heap,
            make_atom(self.peer),
            pid.number() as u32,
            pid.serial() as u32,
            peer_creation,
        )
        .expect("materialize the external pid thing")
    }
}

// ---------------------------------------------------------------------------
// The exit reasons, spelled once and materialised in each side's namespace
// ---------------------------------------------------------------------------

/// The exit reason a case signals. A reason has no cross-node identity of its own — only its
/// *name* crosses the wire — so it is materialised separately on each side
/// ([`Reason::materialise`]) and the decoded term is compared against the receiver's own
/// spelling.
#[derive(Clone, Copy, Debug)]
enum Reason {
    /// The atom `kill`: the untrappable sentinel of the explicit `exit/2` family
    /// (`erl_proc_sig_queue.c:4740`).
    Kill,
    /// The atom `normal`: ignored by a non-trapping target, trapped by a trapping one.
    Normal,
    /// The atom `boom`: an ordinary, trappable exit reason.
    Boom,
    /// The tuple `{boom, 42}`: a **boxed** reason, so the ETF codec really does encode and
    /// decode a compound term inside the control message (and the receiver really does build
    /// the trapped `{'EXIT', …}` around it).
    BoomTuple,
}

impl Reason {
    /// Writes this reason with `heap`/`atoms` — the namespace of whichever node will hold the
    /// resulting term.
    fn materialise(self, heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
        match self {
            Reason::Kill => make_atom(atoms.intern("kill")),
            Reason::Normal => make_atom(atoms.intern("normal")),
            Reason::Boom => make_atom(atoms.intern("boom")),
            Reason::BoomTuple => heap
                .tuple(&[make_atom(atoms.intern("boom")), make_small(42)])
                .expect("a two-element tuple fits the scratch arena"),
        }
    }
}

// ---------------------------------------------------------------------------
// The host seams the two ends install
// ---------------------------------------------------------------------------

/// A distribution seam that records every routed signal instead of touching a socket —
/// `tests/dist_delivery.rs`'s `RecordingRouter`. The log is shared through an
/// `Rc<RefCell<_>>` because the seam lives behind a `Box<dyn DistSignalRouter>` inside the
/// table and is not downcastable.
struct RecordingRouter {
    signals: Rc<RefCell<Vec<DistSignal>>>,
}

impl DistSignalRouter for RecordingRouter {
    fn route(&mut self, signal: DistSignal) -> BeamResult<()> {
        self.signals.borrow_mut().push(signal);
        Ok(())
    }
}

/// The process-death observer node B installs, so a termination's **reason** is observable
/// (the pid simply disappears from the table otherwise).
///
/// Every reason used here is an **immediate** (`kill`/`normal`/`boom`, and the receiver's own
/// rewrite of a `kill` to the atom `killed`), so retaining the raw word out of the dying
/// process's heap is sound — a boxed reason would have to be copied first
/// ([`ProcessExit::reason`]).
struct RecordingExits {
    exits: Rc<RefCell<Vec<(ProcessId, Eterm)>>>,
}

impl ProcessExitObserver for RecordingExits {
    fn process_exited(&mut self, exit: &ProcessExit<'_>) {
        self.exits.borrow_mut().push((exit.pid(), exit.reason()));
    }
}

// ---------------------------------------------------------------------------
// The wire: signaller → router → driver → driver → receipt
// ---------------------------------------------------------------------------

/// What one remote `exit/2` left on the receiving side.
struct Inbound {
    /// The classified control message, ready for the route-free receipt primitive.
    delivery: Delivery,
    /// The signaller's pid **as node B sees it**: an external pid naming node A.
    from: Eterm,
    /// The exit reason as node B decoded it (onto node B's scratch arena).
    reason: Eterm,
}

/// Two nodes on either end of a real driver pair, plus the seams that make the emit and the
/// death observable.
///
/// Field order matters: the tables (which own the mailbox terms the receipt builds) are
/// declared **before** the scratch arenas those terms may point into, so a table always drops
/// first. `Runtime`-shaped, the arenas outlive every process anyway; this only keeps the
/// teardown sound.
struct Wire {
    client: ConnectionDriver,
    server: ConnectionDriver,
    /// The dialer / signaller.
    a: Node,
    /// The listener / recipient.
    b: Node,
    /// Node A's scratch arena (terms node A writes: the pid images and the reason).
    heap_a: Heap,
    /// Node B's scratch arena (terms the inbound decode produces).
    heap_b: Heap,
    /// Every signal node A's table handed to its routing seam.
    signals: Rc<RefCell<Vec<DistSignal>>>,
    /// Every process death node B's table reported, with its exit reason.
    exits: Rc<RefCell<Vec<(ProcessId, Eterm)>>>,
}

impl Wire {
    /// Establishes the link and installs both recording seams.
    fn new() -> Wire {
        let (client, mut server) = establish();
        let mut a = Node::new(CLIENT_NODE, SERVER_NODE, CLIENT_CREATION);
        let mut b = Node::new(SERVER_NODE, CLIENT_NODE, SERVER_CREATION);

        // The runtime installs its own identity on every link it drains
        // (`DistConnection`'s drain), which is what demotes a wire pid naming this node into
        // the internal pid (`dec_is_this_node`) — without it the sink would (correctly)
        // refuse the addressee as a foreign pid.
        server.set_local_identity(LocalIdentity::new(b.name, b.creation));

        let signals = Rc::new(RefCell::new(Vec::new()));
        a.table.set_dist_router(Box::new(RecordingRouter {
            signals: Rc::clone(&signals),
        }));
        let exits = Rc::new(RefCell::new(Vec::new()));
        b.table.set_process_exit_observer(Box::new(RecordingExits {
            exits: Rc::clone(&exits),
        }));

        Wire {
            client,
            server,
            a,
            b,
            heap_a: Heap::new(HEAP_WORDS).expect("node A's scratch arena"),
            heap_b: Heap::new(HEAP_WORDS).expect("node B's scratch arena"),
            signals,
            exits,
        }
    }

    /// One remote `erlang:exit/2`, end to end: `from` (a process of node A) signals `to` (a
    /// process of node B) with `reason`.
    ///
    /// 1. the emitting side — `ProcessTable::send_exit_term` (what `erlang:exit/2`'s remote
    ///    arm calls) must answer `Ok(())` and hand the routing seam **exactly one**
    ///    `DistSignal::Exit2`;
    /// 2. the wire — the signal is spelled exactly as `dist_client`'s `encode_signal` spells a
    ///    `DOP_EXIT2` (`{DOP_EXIT2, From(this node's external pid), To(remote), Reason}`) and
    ///    the real client driver frames it, the real server driver decodes it;
    /// 3. the receiving side — the classified `Delivery::Exit2` reproduces `{from, to, reason}`
    ///    term for term: `from` arrives as an external pid naming **node A** (the signaller is
    ///    on another node), `to` demotes back into node B's own internal pid, and the reason
    ///    compares equal to the receiver's own spelling of it (name- and field-based, so a
    ///    differing atom index cannot mask a divergence).
    fn exit2_over_the_wire(&mut self, from: ProcessId, to: ProcessId, reason: Reason) -> Inbound {
        // --- (1) the emitting side: `erlang:exit/2` against a pid of another node ---------
        let to_term = self.b.peer_pid_term(&mut self.heap_a, to, self.b.creation);
        let reason_a = reason.materialise(&mut self.heap_a, &mut self.a.atoms);
        self.a
            .table
            .send_exit_term(from, to_term, reason_a)
            .expect("a remote `exit/2` routes through the installed signal router");

        let mut signals = std::mem::take(&mut *self.signals.borrow_mut());
        assert_eq!(
            signals.len(),
            1,
            "one remote `erlang:exit/2` must emit exactly one routed signal"
        );
        let signal = signals.pop().expect("one recorded signal");
        let DistSignal::Exit2 {
            from: signalled_from,
            to: signalled_to,
            reason: signalled_reason,
        } = signal
        else {
            panic!("a remote `erlang:exit/2` must emit `DistSignal::Exit2`, got {signal:?}");
        };
        assert_eq!(
            signalled_from, from,
            "the signal names the calling process as its signaller"
        );
        assert_eq!(
            name_of(&self.a.atoms, signalled_to.node()),
            Some(SERVER_NODE),
            "the signal addresses node B"
        );
        assert_eq!(signalled_to.number(), to.number() as u32);
        assert_eq!(signalled_to.serial(), to.serial() as u32);
        assert_eq!(
            signalled_to.creation(),
            SERVER_CREATION,
            "the signal addresses node B's incarnation"
        );
        assert!(
            eq(signalled_reason, reason_a, &self.a.atoms),
            "the emitted signal carries the reason `erlang:exit/2` was given"
        );

        // --- (2) the wire: exactly the `encode_signal` spelling of `DOP_EXIT2` ------------
        let from_term = self.a.pid_term(&mut self.heap_a, from);
        let to_term = self.b.peer_pid_term(&mut self.heap_a, to, self.b.creation);
        self.client
            .send_control(
                Dop::Exit2,
                &[from_term, to_term, reason_a],
                &self.a.atoms,
            )
            .expect("queue the DOP_EXIT2 control message");
        relay(&mut self.client, &mut self.server);

        // --- (3) the receiving side: decode and classify ---------------------------------
        let delivery = self
            .server
            .next_delivery(&mut self.heap_b, &mut self.b.atoms)
            .expect("decode the inbound control message")
            .expect("the server driver queued one control message");

        match &delivery {
            Delivery::Exit2 {
                from: wire_from,
                to: wire_to,
                reason: wire_reason,
                token,
            } => {
                assert!(
                    token.is_none(),
                    "the plain `DOP_EXIT2` form carries no trace token"
                );
                let signaller =
                    RemotePid::from_eterm(*wire_from).expect("`From` decodes to an external pid");
                assert_eq!(
                    name_of(&self.b.atoms, signaller.node()),
                    Some(CLIENT_NODE),
                    "`From` is the signaller's pid on node A"
                );
                assert_eq!(
                    signaller.creation(),
                    CLIENT_CREATION,
                    "`From` names node A's incarnation"
                );
                assert_eq!(
                    signaller.number(),
                    from.number() as u32,
                    "`From` carries the signaller's process-table number"
                );
                assert_eq!(
                    signaller.serial(),
                    from.serial() as u32,
                    "`From` carries the signaller's pid serial"
                );

                let recipient =
                    ProcessId::from_eterm(*wire_to).expect("`To` decodes to an internal pid");
                assert_eq!(
                    recipient, to,
                    "`To` round-trips to the very same local process: node B's identity demotes \
                     its own wire pid back into the internal one (`dec_is_this_node`)"
                );

                let expected = reason.materialise(&mut self.heap_b, &mut self.b.atoms);
                assert!(
                    eq(*wire_reason, expected, &self.b.atoms),
                    "the reason survives the wire term for term"
                );

                Inbound {
                    from: *wire_from,
                    reason: *wire_reason,
                    delivery,
                }
            }
            other => panic!("a `DOP_EXIT2` must classify as `Delivery::Exit2`, got {other:?}"),
        }
    }

    /// Feeds a decoded delivery to table B's **route-free receipt primitive** through the
    /// same [`LocalDeliverySink`] the runtime installs (`apply_remote_exit_signal`).
    fn receipt(&mut self, delivery: &Delivery) -> DeliveryOutcome {
        let mut sink = LocalDeliverySink::new(&mut self.b.table, self.b.name, self.b.creation);
        sink.deliver(delivery)
            .expect("the sink routes without an internal error")
    }

    /// Every process death node B's table reported so far, oldest first.
    fn deaths(&self) -> Vec<(ProcessId, Eterm)> {
        self.exits.borrow().clone()
    }
}

/// The name of `term`, which must be an atom, in `atoms`.
fn name_of<'a>(atoms: &'a AtomTable, term: Eterm) -> Option<&'a str> {
    atoms.name(atom_val(term))
}

/// Every `(payload, envelope sender)` pair `pid`'s mailbox holds, in FIFO order.
fn mailbox(table: &ProcessTable, pid: ProcessId) -> Vec<(Eterm, Eterm)> {
    table
        .get(pid)
        .expect("the addressed process is live")
        .mailbox()
        .iter()
        .map(|message| (message.payload(), message.from()))
        .collect()
}

// ---------------------------------------------------------------------------
// (1) `kill` — the untrappable sentinel of the explicit family
// ---------------------------------------------------------------------------

/// `exit(RemotePid, kill)` kills the remote target and the reason is rewritten to `killed`:
/// no `trap_exit` flag can hold against it, and no message is ever enqueued
/// (`erl_proc_sig_queue.c:4768`).
#[test]
fn a_remote_exit_kill_terminates_the_target_through_the_wire() {
    let mut wire = Wire::new();
    let from = wire.a.pid;
    let victim = wire.b.spawn();

    let inbound = wire.exit2_over_the_wire(from, victim, Reason::Kill);
    assert_eq!(
        wire.receipt(&inbound.delivery),
        DeliveryOutcome::Delivered,
        "an inbound `DOP_EXIT2` is applied, never refused"
    );
    assert!(
        !wire.b.table.contains(victim),
        "`kill` terminates a non-trapping target"
    );
    assert!(
        wire.b.table.get(victim).is_none(),
        "the dead pid resolves to nothing — it is gone from the table, not merely idle"
    );

    let deaths = wire.deaths();
    assert_eq!(deaths.len(), 1, "the target died exactly once");
    assert_eq!(deaths[0].0, victim);
    assert_eq!(
        name_of(&wire.b.atoms, deaths[0].1),
        Some("killed"),
        "an explicit `kill` is rewritten to the exit reason `killed`"
    );
}

/// `kill` is untrappable: a target with `process_flag(trap_exit, true)` dies all the same —
/// the trap arm is guarded by `family == Explicit && class == Kill`
/// (`erl_proc_sig_queue.c:4740`).
#[test]
fn a_remote_exit_kill_is_untrappable_even_for_a_trapping_target() {
    let mut wire = Wire::new();
    let from = wire.a.pid;
    let target = wire.b.spawn();
    wire.b
        .table
        .get_mut(target)
        .expect("the target is live")
        .set_trap_exit(true);

    let inbound = wire.exit2_over_the_wire(from, target, Reason::Kill);
    assert_eq!(
        wire.receipt(&inbound.delivery),
        DeliveryOutcome::Delivered,
        "the untrappable family is applied, not refused"
    );
    assert!(
        !wire.b.table.contains(target),
        "`kill` reaches a trapping target too"
    );

    let deaths = wire.deaths();
    assert_eq!(deaths.len(), 1, "the trapping target died exactly once");
    assert_eq!(deaths[0].0, target);
    assert_eq!(
        name_of(&wire.b.atoms, deaths[0].1),
        Some("killed"),
        "even a trapping target's `kill` is rewritten to `killed`"
    );
}

// ---------------------------------------------------------------------------
// (2) `normal` — ignored by a non-trapping target
// ---------------------------------------------------------------------------

/// `exit(RemotePid, normal)` is a no-op for a non-trapping target: the signal is *applied*
/// (delivered, not refused) and leaves the process running with an empty mailbox
/// (`erl_proc_sig_queue.c:4758`).
#[test]
fn a_remote_exit_normal_leaves_a_non_trapping_target_alive() {
    let mut wire = Wire::new();
    let from = wire.a.pid;
    let victim = wire.b.spawn();

    let inbound = wire.exit2_over_the_wire(from, victim, Reason::Normal);
    assert_eq!(
        wire.receipt(&inbound.delivery),
        DeliveryOutcome::Delivered,
        "a `normal` exit is applied (as a no-op), never refused"
    );
    assert!(
        wire.b.table.contains(victim),
        "a `normal` exit must not terminate a non-trapping target"
    );
    assert!(
        mailbox(&wire.b.table, victim).is_empty(),
        "an exit signal is applied, never delivered as an ordinary message"
    );
    assert!(
        wire.deaths().is_empty(),
        "nothing died, so the observer reported nothing"
    );
}

// ---------------------------------------------------------------------------
// (3) the trapping target — `{'EXIT', From, Reason}`
// ---------------------------------------------------------------------------

/// A trapping target survives and receives exactly `{'EXIT', From, Reason}`, where `From` is
/// the signaller's **external pid** (the signaller is on another node) and `Reason` is the
/// reason the wire carried — here a compound one, `{boom, 42}`, so the ETF codec really does
/// encode and decode the reason element.
///
/// The message's **envelope sender** is the signaller's *node atom*, the C's own choice
/// (`handle_exit_signal` stores the remote node as the sender while the pid stays in the
/// body).
#[test]
fn a_trapping_target_receives_the_exit_message_instead_of_dying() {
    let mut wire = Wire::new();
    let from = wire.a.pid;
    let target = wire.b.spawn();
    wire.b
        .table
        .get_mut(target)
        .expect("the target is live")
        .set_trap_exit(true);

    let inbound = wire.exit2_over_the_wire(from, target, Reason::BoomTuple);
    // The expectation is built by hand, in node B's namespace, out of the terms the wire
    // actually carried — never out of the module under test's own builder.
    let expected = wire
        .heap_b
        .tuple(&[
            make_atom(wire.b.atoms.intern("EXIT")),
            inbound.from,
            inbound.reason,
        ])
        .expect("write `{'EXIT', From, Reason}` on node B's arena");

    assert_eq!(
        wire.receipt(&inbound.delivery),
        DeliveryOutcome::Delivered,
        "a trappable exit is applied, not refused"
    );
    assert!(
        wire.b.table.contains(target),
        "a trapping target survives the exit signal"
    );
    assert!(
        wire.deaths().is_empty(),
        "nothing died, so the observer reported nothing"
    );

    let messages = mailbox(&wire.b.table, target);
    assert_eq!(
        messages.len(),
        1,
        "exactly one `{{'EXIT', From, Reason}}` message, got {messages:?}"
    );
    let (payload, envelope_sender) = messages[0];
    assert!(
        eq(payload, expected, &wire.b.atoms),
        "the trapped exit is `{{'EXIT', From, Reason}}` with the signaller's external pid \
         and the reason the wire carried"
    );
    assert_eq!(
        name_of(&wire.b.atoms, envelope_sender),
        Some(CLIENT_NODE),
        "the message's envelope sender is the signaller's node atom"
    );
}

// ---------------------------------------------------------------------------
// (4) the honest negative: a recipient this node does not hold
// ---------------------------------------------------------------------------

/// An `exit/2` for a process that is already dead is `NoSuchProcess` — the receipt never
/// resurrects a slot and never applies an exit to the wrong process
/// (`erl_proc_sig_queue.c`'s unknown-target arm).
#[test]
fn a_remote_exit_for_a_dead_target_is_no_such_process() {
    let mut wire = Wire::new();
    let from = wire.a.pid;
    let target = wire.b.spawn();
    wire.b
        .table
        .terminate(target, make_atom(wire.b.atoms.intern("boom")))
        .expect("the local target dies");
    assert!(!wire.b.table.contains(target));
    let before = wire.deaths().len();

    let inbound = wire.exit2_over_the_wire(from, target, Reason::Boom);
    assert_eq!(
        wire.receipt(&inbound.delivery),
        DeliveryOutcome::NoSuchProcess,
        "an exit for a pid this table no longer holds is reported, not silently applied"
    );
    assert_eq!(
        wire.deaths().len(),
        before,
        "the refused exit killed nothing"
    );
}
