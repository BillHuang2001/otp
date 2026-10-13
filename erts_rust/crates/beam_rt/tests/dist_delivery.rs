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

//! The runtime side of the **inbound distribution delivery** seam (contract A.5):
//! a decoded, classified `DOP_*` reaches the addressed **local** process's mailbox
//! — or is reported honestly as unroutable — and never anywhere else.
//!
//! `beam_dist` owns the wire and the classification (`beam_dist::delivery`): a
//! [`ConnectionDriver`] frames a link and turns an inbound control message into a
//! [`Delivery`](beam_dist::Delivery). `beam_rt` owns the *routing*: the
//! [`LocalDeliverySink`](beam_rt::dist::LocalDeliverySink) resolves the addressee
//! against this runtime's process table and enqueues the message through
//! `beam_process`'s own delivery path.
//!
//! Every test here is **hermetic**: a client and a server [`ConnectionDriver`] are
//! wired to each other and the client's [`ConnectionDriver::take_output`] bytes are
//! shuttled by hand (the harness of the peer crate's `tests/delivery.rs`), so no
//! socket, no `epmd` and no system `erl` is needed. The last test drives a **real
//! loopback TCP link** into a running runtime through the scheduler pass — still
//! hermetic (no external toolchain).
//!
//! The `(7)` group goes one level lower: synthetic [`Delivery`] values are driven
//! straight into the public [`LocalDeliverySink`] over a **standalone**
//! [`ProcessTable`], pinning the routing table one family at a time (each routed
//! family by its delivered effect, each unrouted family by its honest
//! `Unsupported`).
//!
//! Small integers are used as the payload wherever the exact term matters: unlike
//! atoms (whose raw encoding is table-local), a small integer is its own immediate
//! value, so a term compares `==` across heaps.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::make_external_pid;
use beam_dist::flags::Dop;
use beam_dist::{
    AltactSigFlags, ConnectionDriver, Delivery, DeliveryOutcome, DistSpawnFlags, DriverState,
    InboundSink, StepOutcome,
};
use beam_process::{ProcessId, ProcessTable, SignalAtoms, UnaliasMode};
use beam_rt::dist::LocalDeliverySink;
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::prelude::{is_immediate, make_atom, make_small, Atom, Eterm};
use beam_term::AtomTable;

/// The shared cookie both ends authenticate with.
const COOKIE: &[u8] = b"s3cr3t-cookie";
/// The connecting node's name.
const CLIENT_NAME: &[u8] = b"client@host";
/// The connecting node's creation.
const CLIENT_CREATION: u32 = 7;
/// A scratch arena big enough for every payload these tests build.
const HEAP_WORDS: usize = 1 << 16;

// ---------------------------------------------------------------------------
// The hermetic two-driver harness (mirrors `beam_dist/tests/delivery.rs`)
// ---------------------------------------------------------------------------

/// Drives a client and a server driver against each other until both are
/// established.
fn establish() -> (ConnectionDriver, ConnectionDriver) {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
    let mut server = ConnectionDriver::server(b"server@host", COOKIE, 0xdead_beef);
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

/// A runtime that **is** a distributed node (a bound listener, no `epmd` needed for
/// this test), named uniquely so parallel test binaries cannot collide.
fn node_runtime(tag: &str) -> Runtime {
    let name = format!("beamrtdeliv{}{}", tag, std::process::id());
    let args: Vec<String> = [
        "-noshell",
        "-sname",
        &name,
        "-setcookie",
        "delivery-cookie",
        "-eval",
        "ok.",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).expect("the node switches parse");
    Runtime::new(config).expect("the runtime starts (the listener binds)")
}

/// The addressed pid term for this runtime's own root process, as it arrives on the
/// wire: an **external** pid (`NEW_PID_EXT`) whose node/creation are this node's.
///
/// The distribution decoder always materialises a pid as an external pid thing (it
/// carries the peer's node name), so this is exactly the shape the sink must accept.
fn own_pid_term(rt: &mut Runtime, heap: &mut Heap) -> Eterm {
    let (name, creation) = {
        let node = rt.dist_node().expect("a configured node");
        (node.node_name().to_string(), node.creation())
    };
    let node_atom = rt.intern(&name);
    let pid = rt.self_pid();
    make_external_pid(
        heap,
        node_atom,
        pid.number() as u32,
        pid.serial() as u32,
        creation,
    )
    .expect("materialize the addressed external pid")
}

/// An external pid on a **different** node: never this runtime's process.
fn foreign_pid_term(rt: &mut Runtime, heap: &mut Heap) -> Eterm {
    let atom = rt.intern("somewhere-else@host");
    make_external_pid(heap, atom, 1, 0, 0x1234_5678).expect("materialize a foreign pid")
}

/// The single message in `pid`'s mailbox, or `None` when it is empty.
fn only_message(rt: &Runtime, pid: ProcessId) -> Option<Eterm> {
    let process = rt.processes().get(pid)?;
    let mut messages = process.mailbox().iter();
    let first = messages.next()?.payload();
    assert!(
        messages.next().is_none(),
        "exactly one message must have been delivered"
    );
    Some(first)
}

// ---------------------------------------------------------------------------
// (1) `DOP_SEND` to a local pid reaches that process's mailbox
// ---------------------------------------------------------------------------

#[test]
fn an_inbound_send_reaches_the_addressed_local_mailbox() {
    let mut rt = node_runtime("send");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    let to = own_pid_term(&mut rt, &mut heap);
    let from = make_small(7);
    let message = make_small(42);

    // `send_message` selects `DOP_SEND_SENDER` (the flag OTP negotiates) with the
    // message as the trailing term.
    client
        .send_message(from, to, message, rt.code_server_atoms_mut())
        .expect("queue DOP_SEND");
    relay(&mut client, &mut server);

    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch the inbound send");
    assert_eq!(dispatched.len(), 1, "one control message was queued");
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::Delivered,
        "a send addressed to a local pid must be delivered: {:?}",
        dispatched[0].delivery
    );
    assert_eq!(
        only_message(&rt, rt.self_pid()),
        Some(message),
        "the message must sit in the addressed process's own mailbox"
    );
}

// ---------------------------------------------------------------------------
// (2) `DOP_REG_SEND` to a name this runtime holds reaches that process
// ---------------------------------------------------------------------------

#[test]
fn an_inbound_reg_send_reaches_a_registered_name() {
    let mut rt = node_runtime("regsend");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    // `init` is the name `Runtime::new` registers for the runtime's own root
    // process, so it is the one local name a fresh runtime really holds.
    let name = rt.intern("init");
    let from = make_small(11);
    let message = make_small(1234);

    client
        .send_control(
            Dop::RegSend,
            &[from, make_small(0), name, message],
            rt.code_server_atoms_mut(),
        )
        .expect("queue DOP_REG_SEND");
    relay(&mut client, &mut server);

    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch the inbound reg_send");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::Delivered,
        "a reg_send to a registered name must be delivered: {:?}",
        dispatched[0].delivery
    );
    assert_eq!(
        only_message(&rt, rt.self_pid()),
        Some(message),
        "the message must sit in the registered process's mailbox"
    );
}

// ---------------------------------------------------------------------------
// (3) The honest negatives: no wrong delivery, ever
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_registered_name_is_no_such_process() {
    let mut rt = node_runtime("noname");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    let name = rt.intern("nobody-holds-this-name");
    let from = make_small(11);
    let message = make_small(99);

    client
        .send_control(
            Dop::RegSend,
            &[from, make_small(0), name, message],
            rt.code_server_atoms_mut(),
        )
        .expect("queue DOP_REG_SEND");
    relay(&mut client, &mut server);

    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::NoSuchProcess,
        "an unheld name must NOT be delivered anywhere"
    );
    assert_eq!(
        only_message(&rt, rt.self_pid()),
        None,
        "nothing may reach the runtime's own process"
    );
}

#[test]
fn a_pid_on_another_node_is_no_such_process_and_never_mis_delivered() {
    let mut rt = node_runtime("foreign");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    let to = foreign_pid_term(&mut rt, &mut heap);
    let from = make_small(1);
    let message = make_small(2);

    client
        .send_message(from, to, message, rt.code_server_atoms_mut())
        .expect("queue DOP_SEND");
    relay(&mut client, &mut server);

    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::NoSuchProcess,
        "a pid on another node is not a process of this runtime"
    );
    assert_eq!(
        only_message(&rt, rt.self_pid()),
        None,
        "nothing may be delivered for a foreign pid (the slot number alone is not an identity)"
    );
}

// ---------------------------------------------------------------------------
// (4) The unrouted families are the honest `Unsupported`, never a substituted send
// ---------------------------------------------------------------------------

#[test]
fn an_alias_send_is_unsupported_not_a_wrong_delivery() {
    let mut rt = node_runtime("alias");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    // `DOP_ALIAS_SEND`'s `To` must be a **reference** (`dist.c:2577`
    // `is_not_ref(to)` ⇒ `invalid_message`); a small integer is outside the
    // opcode's wire contract, so the sink must refuse it rather than deliver to
    // whichever pid the *number* happens to name. (A genuinely unowned reference is
    // `Dropped` — see the routing-logic cases below.)
    let to = make_small(7);
    let from = make_small(1);
    let message = make_small(2);
    client
        .send_control(
            Dop::AliasSend,
            &[from, to, message],
            rt.code_server_atoms_mut(),
        )
        .expect("queue DOP_ALIAS_SEND");
    relay(&mut client, &mut server);

    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::Unsupported,
        "an alias send has no correct local seam: {:?}",
        dispatched[0].delivery
    );
    assert_eq!(only_message(&rt, rt.self_pid()), None);
}

#[test]
fn the_signal_families_are_unsupported_without_a_correct_local_seam() {
    let mut rt = node_runtime("signal");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    let from = make_small(1);
    let to = make_small(2);
    let reason = make_small(3);

    // Link, exit and the monitor family all address *local* parties in
    // `beam_process`'s own structures, which accept only local peers — there is no
    // seam to install a remote counterpart, so each is an honest `Unsupported`.
    let cases: [(Dop, &[Eterm]); 3] = [
        (Dop::Link, &[from, to]),
        (Dop::Exit, &[from, to, reason]),
        (Dop::MonitorP, &[from, to, reason]),
    ];
    for (dop, terms) in cases {
        client
            .send_control(dop, terms, rt.code_server_atoms_mut())
            .expect("queue control");
        relay(&mut client, &mut server);
        let dispatched = rt
            .dispatch_inbound(&mut heap, &mut server)
            .expect("dispatch");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(
            dispatched[0].outcome,
            DeliveryOutcome::Unsupported,
            "{dop:?} has no correct local seam: {:?}",
            dispatched[0].delivery
        );
    }
    assert_eq!(only_message(&rt, rt.self_pid()), None);
}

// ---------------------------------------------------------------------------
// (5) The scheduler-pass gate: routing is ON by default and can be switched off
// ---------------------------------------------------------------------------

#[test]
fn inbound_routing_is_on_by_default_and_toggleable() {
    let mut rt = node_runtime("gate");
    assert!(
        rt.inbound_delivery_enabled(),
        "a live node must deliver what a peer sends it by default — that is what accepting the peer meant"
    );
    rt.allow_inbound_delivery(false);
    assert!(
        !rt.inbound_delivery_enabled(),
        "the explicit off switch must still keep a node's links alive while routing nothing"
    );
    rt.allow_inbound_delivery(true);
    assert!(rt.inbound_delivery_enabled());
}

/// Flipping the default cannot regress a run without distribution traffic: a
/// hermetic `nonode@nohost` runtime has no node to dispatch for, so the pass's
/// inbound half stays a no-op.
#[test]
fn a_nodeless_runtime_still_reports_no_inbound_traffic() {
    let config = beam_rt::parse(&["-noshell".to_string(), "-eval".to_string(), "ok.".to_string()])
        .expect("the default switches parse");
    let mut rt = Runtime::new(config).expect("a hermetic runtime starts");
    assert!(rt.dist_node().is_none(), "no `-name`/`-sname` ⇒ no node");
    rt.run_one_slice().expect("a scheduler pass");
}

// ---------------------------------------------------------------------------
// (6) End to end through a **real loopback link** and the scheduler pass
// ---------------------------------------------------------------------------

/// How long the live loopback link is given to handshake and deliver.
const LIVE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a single non-blocking write may take to drain.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to sleep between polls when the socket is not ready.
const POLL_SLEEP: Duration = Duration::from_millis(2);

/// Writes `bytes` in full to a non-blocking socket, retrying `WouldBlock`.
fn write_all_nonblocking(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    let deadline = Instant::now() + WRITE_TIMEOUT;
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::WriteZero, "0 bytes")),
            Ok(n) => written += n,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "the non-blocking write did not drain in time",
                    ));
                }
                std::thread::sleep(POLL_SLEEP);
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// One round of a hand-driven non-blocking link: step to quiescence, write what the
/// driver queued, then read whatever arrived.
fn round(stream: &mut TcpStream, driver: &mut ConnectionDriver) {
    loop {
        match driver.step() {
            Ok(StepOutcome::WouldBlock) | Ok(StepOutcome::Closed) | Ok(StepOutcome::Failed) => {
                break
            }
            Ok(_) => {}
            Err(err) => panic!("the driver reported a protocol error: {err:?}"),
        }
    }
    let pending = driver.take_output();
    if !pending.is_empty() {
        write_all_nonblocking(stream, &pending).expect("write the driver's output");
    }
    let mut buf = vec![0u8; 64 * 1024];
    match stream.read(&mut buf) {
        Ok(0) => driver.feed_eof(),
        Ok(n) => driver.feed(&buf[..n]),
        Err(err) if err.kind() == ErrorKind::WouldBlock => {}
        Err(err) if err.kind() == ErrorKind::Interrupted => {}
        Err(err) => panic!("socket read failed: {err}"),
    }
}

#[test]
fn a_real_loopback_link_delivers_through_the_scheduler_pass() {
    let mut rt = node_runtime("live");
    rt.allow_inbound_delivery(true);

    // One pass publishes the node and opens the listener for accepting.
    rt.run_one_slice().expect("a scheduler pass");
    let (port, name, creation) = {
        let node = rt.dist_node().expect("a configured node");
        (node.port(), node.node_name().to_string(), node.creation())
    };

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the listener");
    stream.set_nonblocking(true).expect("non-blocking socket");
    let mut client = ConnectionDriver::client(CLIENT_NAME, b"delivery-cookie", CLIENT_CREATION);

    let mut heap = Heap::new(HEAP_WORDS).expect("client heap");
    let pid = rt.self_pid();
    let mut client_atoms = AtomTable::new();
    let node_atom = make_atom(client_atoms.intern(&name));
    let to = make_external_pid(
        &mut heap,
        node_atom,
        pid.number() as u32,
        pid.serial() as u32,
        creation,
    )
    .expect("materialize the addressed pid");
    let message = make_small(4242);

    let deadline = Instant::now() + LIVE_TIMEOUT;
    let mut sent = false;
    loop {
        round(&mut stream, &mut client);
        if !sent && client.state() == DriverState::Established {
            client
                .send_message(make_small(1), to, message, &client_atoms)
                .expect("queue the send");
            sent = true;
        }
        // Drive the runtime's own pass: `pump_dist` reads the link, the inbound half
        // routes what it decoded.
        rt.run_one_slice().expect("a scheduler pass");
        if only_message(&rt, pid) == Some(message) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the loopback link never delivered the message within {LIVE_TIMEOUT:?}"
        );
        std::thread::sleep(POLL_SLEEP);
    }
    assert_eq!(
        only_message(&rt, pid),
        Some(message),
        "the scheduled pass must route an inbound send into the local mailbox"
    );
}

// ---------------------------------------------------------------------------
// (7) The routing table, driven **per family** through the public sink
// ---------------------------------------------------------------------------
//
// The tests above reach the routing through the wire. These drive synthetic
// [`Delivery`] values straight into a [`LocalDeliverySink`] over a standalone
// [`ProcessTable`] — no runtime, no socket, no `epmd` — so every arm of the routing
// match is pinned individually: each *routed* family by its delivered effect, and
// every *unrouted* family by the honest `Unsupported`. A future change that starts
// routing one of those families must therefore flip a row **consciously**.

/// This test node's name; it is the sink's `local_node` atom.
const NODE_NAME: &str = "delivery-under-test@host";
/// A **different** node's name, interned alongside [`NODE_NAME`] so the two atom
/// indices are distinct by construction.
const FOREIGN_NODE_NAME: &str = "somewhere-else@host";
/// This test node's creation (the incarnation an addressed external pid must carry to
/// be a process of this node).
const NODE_CREATION: u32 = 0x0123_4567;

/// A **hermetic** routing fixture: a standalone process table, one spawned local
/// process, and the atom table the fixture's atoms live in.
struct Fixture {
    /// The process table the sink routes into.
    table: ProcessTable,
    /// One spawned local process — the default addressee of these tests.
    pid: ProcessId,
    /// This node's name atom (the sink's `local_node`).
    local_node: Atom,
    /// A **different** node's name atom.
    foreign_node: Atom,
    /// The atom table `local_node`/`foreign_node` (and any name a test interns) live
    /// in — the same table, so no index can collide across fixtures.
    atoms: AtomTable,
}

impl Fixture {
    /// A table with one spawned process, for a node named [`NODE_NAME`]/[`NODE_CREATION`].
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        let local_node = atoms.intern(NODE_NAME);
        let foreign_node = atoms.intern(FOREIGN_NODE_NAME);
        let signals = SignalAtoms::from_table(&mut atoms);
        let mut table = ProcessTable::new(signals);
        let pid = table.spawn_default().expect("spawn a local process");
        Fixture {
            table,
            pid,
            local_node,
            foreign_node,
            atoms,
        }
    }

    /// Spawns one more local process in this fixture's table.
    fn spawn(&mut self) -> ProcessId {
        self.table.spawn_default().expect("spawn a local process")
    }

    /// The addressed pid term for `pid` as it arrives **from this node**: an external
    /// pid thing — the shape the distribution decoder always materialises a pid as.
    fn own_pid_term(&self, heap: &mut Heap, pid: ProcessId) -> Eterm {
        external_pid_term(
            heap,
            make_atom(self.local_node),
            pid.number(),
            pid.serial(),
            NODE_CREATION,
        )
    }
}

/// Writes an **external** pid thing for `node`/`number`/`serial`/`creation`.
fn external_pid_term(
    heap: &mut Heap,
    node: Eterm,
    number: u64,
    serial: u64,
    creation: u32,
) -> Eterm {
    make_external_pid(heap, node, number as u32, serial as u32, creation)
        .expect("materialize the external pid thing")
}

/// Routes `delivery` through a `LocalDeliverySink` over `table` for the node named by
/// `node` with [`NODE_CREATION`].
fn route(table: &mut ProcessTable, node: Atom, delivery: &Delivery) -> DeliveryOutcome {
    let mut sink = LocalDeliverySink::new(table, node, NODE_CREATION);
    sink.deliver(delivery)
        .expect("the sink routes without an internal error")
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

// --- `DOP_SEND*` ------------------------------------------------------------

/// An **internal** pid immediate is a local process by construction, so a `DOP_SEND`
/// to it reaches its mailbox.
#[test]
fn an_internal_pid_send_target_is_routed_to_its_mailbox() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let from = make_small(7);
    let message = make_small(42);

    let delivery = Delivery::Send {
        from,
        to: pid.to_eterm(),
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );
    assert_eq!(mailbox(&fixture.table, pid), vec![(message, from)]);
}

/// An **external** pid whose node atom and creation are *this* node's names a local
/// process — looked up by number **and** serial.
#[test]
fn an_external_pid_on_this_node_is_routed_to_its_mailbox() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let to = fixture.own_pid_term(&mut heap, pid);
    let from = make_small(7);
    let message = make_small(42);

    let delivery = Delivery::Send {
        from,
        to,
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );
    assert_eq!(mailbox(&fixture.table, pid), vec![(message, from)]);
}

/// A pid on **another node**, or a stale incarnation of this one, is never resolved
/// by its slot number alone (the C drops such a message silently, `dist.c:2512-2561`).
#[test]
fn an_external_pid_of_another_node_or_incarnation_is_no_such_process() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let foreign_node = fixture.foreign_node;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let from = make_small(7);
    let message = make_small(42);

    let cases = [
        (
            "a pid on another node",
            external_pid_term(
                &mut heap,
                make_atom(foreign_node),
                pid.number(),
                pid.serial(),
                NODE_CREATION,
            ),
        ),
        (
            "a stale incarnation of this node",
            external_pid_term(
                &mut heap,
                make_atom(node),
                pid.number(),
                pid.serial(),
                NODE_CREATION.wrapping_add(1),
            ),
        ),
    ];
    for (what, to) in cases {
        let delivery = Delivery::Send {
            from,
            to,
            message,
            token: None,
        };
        assert_eq!(
            route(&mut fixture.table, node, &delivery),
            DeliveryOutcome::NoSuchProcess,
            "{what} must never be resolved to a process of this node"
        );
    }
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "nothing may be delivered for a foreign pid"
    );
}

/// A `DOP_SEND` whose `To` is not a pid is outside the opcode's wire contract (the C
/// treats it as an `invalid_message`, `dist.c:2512-2561`), so the sink refuses it
/// rather than delivering to whichever process the *number* happens to name.
#[test]
fn a_non_pid_send_target_is_unsupported() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let from = make_small(7);
    let message = make_small(42);

    for to in [make_small(pid.number() as i64), make_atom(node)] {
        let delivery = Delivery::Send {
            from,
            to,
            message,
            token: None,
        };
        assert_eq!(
            route(&mut fixture.table, node, &delivery),
            DeliveryOutcome::Unsupported,
            "a non-pid `To` is outside DOP_SEND's contract"
        );
    }
    assert!(mailbox(&fixture.table, pid).is_empty());
}

/// **The envelope regression.** A `DOP_SEND_SENDER*` sender pid arrives as a *boxed*
/// external pid thing in the dispatch arena, which is reset after every drain, so
/// storing it in the mailbox envelope would leave a dangling word (the collector
/// gathers only each message's **payload**, `beam_message`'s enqueue tripwire). The
/// sink must derive the peer **node atom** — an immediate — exactly as the C's
/// `ERL_MESSAGE_FROM` rule does (`erl_message.c:307`), and the delivery must still
/// read back after the arena is rewound.
#[test]
fn a_send_sender_envelope_is_an_immediate_that_survives_the_arena_reset() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");

    // The `From` of a `DOP_SEND_SENDER`: a boxed external pid on *this* node.
    let from = external_pid_term(&mut heap, make_atom(node), 9, 3, NODE_CREATION);
    let message = make_small(4242);
    let delivery = Delivery::Send {
        from,
        to: pid.to_eterm(),
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );

    let delivered = mailbox(&fixture.table, pid);
    assert_eq!(delivered.len(), 1, "exactly one message was delivered");
    assert_eq!(
        delivered[0].0, message,
        "the payload must sit in the addressed process's mailbox"
    );
    assert!(
        is_immediate(delivered[0].1),
        "the envelope's sender word must be an **immediate**, never the boxed sender \
         pid — which would dangle into the evacuated arena"
    );
    assert_eq!(
        delivered[0].1,
        make_atom(node),
        "the envelope's sender word is the sending pid's node atom, the C's own \
         ERL_MESSAGE_FROM rule"
    );

    // Rewind the decode arena — every boxed word built above is gone — and read the
    // mailbox back: what the sink stored must not point into the arena at all.
    heap.reset();
    let after = mailbox(&fixture.table, pid);
    assert_eq!(
        after, delivered,
        "the mailbox must be unaffected by the decode arena's reset"
    );
    assert!(is_immediate(after[0].1));
}

// --- `DOP_REG_SEND*` --------------------------------------------------------

/// `DOP_REG_SEND` routes a registered name to its holder, and refuses a non-atom name
/// (the opcode's `ToName` is an atom, `dist.c:2486`).
#[test]
fn a_reg_send_routes_a_registered_name_and_refuses_a_non_atom_name() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let name = fixture.atoms.intern("delivery-fixture-name");
    fixture.table.register(name, pid).expect("register the name");
    let from = make_small(11);
    let message = make_small(1234);

    let delivery = Delivery::RegSend {
        from,
        cookie: make_small(0),
        name: make_atom(name),
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );
    assert_eq!(mailbox(&fixture.table, pid), vec![(message, from)]);

    let non_atom = Delivery::RegSend {
        from,
        cookie: make_small(0),
        name: make_small(7),
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &non_atom),
        DeliveryOutcome::Unsupported,
        "a non-atom `ToName` is outside DOP_REG_SEND's contract"
    );
    assert_eq!(
        mailbox(&fixture.table, pid),
        vec![(message, from)],
        "the refused delivery must add nothing"
    );
}

/// An unheld name is a silent drop in the C (`dist.c:2506-2509`) — never a delivery to
/// a same-indexed process.
#[test]
fn an_unregistered_name_is_no_such_process() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let name = fixture.atoms.intern("nobody-holds-this-name");

    let delivery = Delivery::RegSend {
        from: make_small(11),
        cookie: make_small(0),
        name: make_atom(name),
        message: make_small(99),
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::NoSuchProcess
    );
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "an unheld name must not be delivered anywhere"
    );
}

// --- `DOP_ALIAS_SEND*` ------------------------------------------------------

/// `DOP_ALIAS_SEND` resolves a live reference alias to its **owner**, and a
/// `reply_demonitor` (`ALIAS_ONCE`) alias is **consumed** once the message really went
/// through it (`erl_proc_sig_queue.c:6047-6064`).
#[test]
fn an_alias_send_reaches_the_owner_and_consumes_a_reply_alias() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let reference = fixture
        .table
        .create_alias(pid, UnaliasMode::ReplyDemonitor)
        .expect("mint a reply alias");
    let to = reference.to_eterm(&mut heap).expect("materialize the reference");
    let from = make_small(5);
    let message = make_small(2024);

    let delivery = Delivery::AliasSend {
        from,
        to,
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );
    assert_eq!(
        mailbox(&fixture.table, pid),
        vec![(message, from)],
        "an alias send must reach the alias owner's mailbox"
    );

    // The reply alias is spent: the same reference no longer resolves, and the C drops
    // such a message silently rather than mis-delivering it.
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Dropped,
        "a consumed reply alias must not deliver a second time"
    );
    assert_eq!(
        mailbox(&fixture.table, pid),
        vec![(message, from)],
        "the second delivery must not arrive"
    );
}

/// A reference that is not a live alias is a silent drop
/// (`erl_proc_sig_queue.c:6036-6046`: *"Not an alias … drop message"*), and a target
/// that is not a reference at all is outside the opcode's contract (`dist.c:2577`).
#[test]
fn an_unowned_reference_is_dropped_and_a_non_ref_target_is_unsupported() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let from = make_small(5);
    let message = make_small(2024);

    // A reference that *was* an alias and is not any more.
    let spent = fixture
        .table
        .create_alias(pid, UnaliasMode::ExplicitUnalias)
        .expect("mint an alias");
    assert!(
        fixture.table.unalias(pid, spent).expect("clear the alias"),
        "the alias really was live before the unalias"
    );
    let spent_term = spent.to_eterm(&mut heap).expect("materialize the reference");
    let dropped = Delivery::AliasSend {
        from,
        to: spent_term,
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &dropped),
        DeliveryOutcome::Dropped,
        "a reference that is no longer an alias must be dropped, not delivered"
    );
    assert!(mailbox(&fixture.table, pid).is_empty());

    // `DOP_ALIAS_SEND`'s `To` must be a reference; a small integer is outside its
    // wire contract, so it is refused rather than resolved by number.
    let non_ref = Delivery::AliasSend {
        from,
        to: make_small(7),
        message,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &non_ref),
        DeliveryOutcome::Unsupported
    );
    assert!(mailbox(&fixture.table, pid).is_empty());
}

// --- `DOP_ALTACT_SIG_SEND` --------------------------------------------------

/// `DOP_ALTACT_SIG_SEND`'s plain **message** forms route exactly like the
/// corresponding `DOP_*` family: `..._ALIAS` ⇒ a reference, `..._NAME` ⇒ a registered
/// name, neither ⇒ a pid (`dist.c:2662-2696`).
#[test]
fn an_altact_sig_send_routes_the_plain_message_forms() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let by_pid = fixture.pid;
    let by_name = fixture.spawn();
    let by_alias = fixture.spawn();
    let name = fixture.atoms.intern("delivery-altact-name");
    fixture.table.register(name, by_name).expect("register the name");
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let alias = fixture
        .table
        .create_alias(by_alias, UnaliasMode::ExplicitUnalias)
        .expect("mint an alias");
    let from = make_small(3);
    let message = make_small(77);

    let cases = [
        (by_pid, AltactSigFlags::EMPTY, by_pid.to_eterm()),
        (by_name, AltactSigFlags::NAME, make_atom(name)),
        (
            by_alias,
            AltactSigFlags::ALIAS,
            alias.to_eterm(&mut heap).expect("materialize the reference"),
        ),
    ];
    for (pid, flags, to) in cases {
        let delivery = Delivery::AltactSigSend {
            flags,
            from,
            to,
            token: None,
            message,
        };
        assert_eq!(
            route(&mut fixture.table, node, &delivery),
            DeliveryOutcome::Delivered,
            "{flags:?} must route the plain message form"
        );
        assert_eq!(mailbox(&fixture.table, pid), vec![(message, from)]);
    }
}

/// A `PRIO` signal needs a priority queue this mailbox has no seam for, and an `EXIT`
/// signal is an (untrappable) exit reason — neither may be delivered as an ordinary
/// message, which would silently reorder the one and re-shape the other.
#[test]
fn an_altact_sig_priority_or_exit_signal_is_unsupported() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let from = make_small(3);
    let message = make_small(77);

    let mut prio_and_exit = AltactSigFlags::PRIO;
    prio_and_exit.insert(AltactSigFlags::EXIT);
    for flags in [AltactSigFlags::PRIO, AltactSigFlags::EXIT, prio_and_exit] {
        let delivery = Delivery::AltactSigSend {
            flags,
            from,
            to: pid.to_eterm(),
            token: None,
            message,
        };
        assert_eq!(
            route(&mut fixture.table, node, &delivery),
            DeliveryOutcome::Unsupported,
            "{flags:?} has no correct local seam"
        );
    }
    assert!(mailbox(&fixture.table, pid).is_empty());
}

// --- the escalation list ----------------------------------------------------

/// **One row per unrouted family**: every family whose local counterpart
/// `beam_process` cannot install is the honest `Unsupported` — never a substituted
/// message, never a dropped signal. This pins the escalation list so a future change
/// must flip a row on purpose rather than by accident.
#[test]
fn every_unrouted_family_is_the_honest_unsupported() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let sender = make_small(1);
    let target = pid.to_eterm();
    let aux = make_small(2);
    let reason = make_small(3);

    let rows: [(Dop, Delivery); 15] = [
        // `DOP_LINK`: needs a record-only `record_remote_link` (the table's only
        // entry point would echo a `DOP_LINK` the C never sends back).
        (Dop::Link, Delivery::Link {
            from: sender,
            to: target,
        }),
        // The remote-sender exit family: `ExitSignal` carries a **local** sender pid.
        (Dop::Exit, Delivery::Exit {
            from: sender,
            to: target,
            reason,
            token: None,
        }),
        (Dop::Exit2, Delivery::Exit2 {
            from: sender,
            to: target,
            reason,
            token: None,
        }),
        (Dop::PayloadExit, Delivery::PayloadExit {
            from: sender,
            to: target,
            payload: reason,
            token: None,
        }),
        (Dop::PayloadExit2, Delivery::PayloadExit2 {
            from: sender,
            to: target,
            payload: reason,
            token: None,
        }),
        // The OTP-26+ unlink-id handshake: an inbound `DOP_UNLINK_ID` must be answered
        // with its `_ACK`, which the local-initiated `unlink_term` never does.
        (Dop::UnlinkId, Delivery::UnlinkId {
            id: aux,
            from: sender,
            to: target,
        }),
        (Dop::UnlinkIdAck, Delivery::UnlinkIdAck {
            id: aux,
            from: sender,
            to: target,
        }),
        // A remote watcher: a monitor's origin side is a local pid here.
        (Dop::MonitorP, Delivery::Monitor {
            watcher: sender,
            watched: target,
            reference: aux,
        }),
        (Dop::DemonitorP, Delivery::Demonitor {
            watcher: sender,
            watched: target,
            reference: aux,
        }),
        // A monitor DOWN from a remote target: firing the local watcher's outgoing
        // record is a local-death path here.
        (Dop::MonitorPExit, Delivery::MonitorExit {
            watched: sender,
            watcher: target,
            reference: aux,
            reason,
        }),
        (Dop::PayloadMonitorPExit, Delivery::PayloadMonitorExit {
            watched: sender,
            watcher: target,
            reference: aux,
            payload: reason,
        }),
        // `DOP_GROUP_LEADER`: the group leader is only settable locally today.
        (Dop::GroupLeader, Delivery::GroupLeader {
            from: sender,
            to: target,
        }),
        // The distributed-spawn family: a CLI process serves no spawn requests and
        // holds no spawn reservations.
        (Dop::SpawnRequest, Delivery::SpawnRequest {
            reference: aux,
            from: sender,
            group_leader: aux,
            mfa: aux,
            opts: aux,
            args: aux,
            token: None,
        }),
        (Dop::SpawnReply, Delivery::SpawnReply {
            reference: aux,
            to: target,
            flags: DistSpawnFlags::EMPTY,
            result: aux,
            token: None,
        }),
        // The one `DOP_*` recognised on the wire but refused by the C outright — it
        // kills the connection rather than classifying it (`dist.c:2277-2282`).
        (Dop::Unlink, Delivery::Other(Dop::Unlink)),
    ];

    for (dop, delivery) in rows {
        assert_eq!(
            delivery.dop(),
            dop,
            "the row's opcode must match the family it stands for"
        );
        assert_eq!(
            route(&mut fixture.table, node, &delivery),
            DeliveryOutcome::Unsupported,
            "{dop:?} has no correct local seam and must say so: {delivery:?}"
        );
    }
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "no unrouted family may have delivered anything"
    );
}
