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
//! [`ProcessTable`], pinning the routing table one family at a time — each routed
//! family by its delivered effect (the ordinary message families by a mailbox
//! delivery, the link/exit/unlink/monitor-down/group-leader signal families by the
//! **route-free receipt** state they install) and each still-unrouted family by its
//! honest `Unsupported`. A receipt family installs (or drops) its local record and
//! echoes nothing, so a recording [`DistSignalRouter`] proves the whole set leaves
//! the wire untouched.
//!
//! Small integers are used as the payload wherever the exact term matters: unlike
//! atoms (whose raw encoding is table-local), a small integer is its own immediate
//! value, so a term compares `==` across heaps.

use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::{make_external_pid, make_external_ref};
use beam_common::Result as BeamResult;
use beam_dist::flags::Dop;
use beam_dist::{
    AltactSigFlags, ConnectionDriver, Delivery, DeliveryOutcome, DistSpawnFlags, DriverState,
    InboundSink, StepOutcome,
};
use beam_process::{
    DistSignal, DistSignalRouter, MonitorKind, PidTarget, ProcessId, ProcessTable, RemotePid,
    SignalAtoms, UnaliasMode,
};
use beam_rt::dist::LocalDeliverySink;
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::prelude::{eq, is_immediate, make_atom, make_small, size_object, Atom, Eterm};
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
// (4) The wire-borne shapes that are not delivered — and the wired receipt families
// ---------------------------------------------------------------------------

#[test]
fn an_alias_send_to_a_wire_borne_reference_is_unsupported_not_a_wrong_delivery() {
    let mut rt = node_runtime("alias");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    // `DOP_ALIAS_SEND`'s `To` must be a **reference** (`dist.c:2577`
    // `is_not_ref(to)` ⇒ `invalid_message`), so a well-formed wire message carries a
    // reference thing. A wire reference is always the **external**
    // (`NEWER_REFERENCE_EXT`) form: the control-message decoder installs no local-node
    // identity, so the codec's local-node demotion never runs and the sink sees an
    // external reference — never a readable local alias key.
    //
    // The sink therefore reports the typed **`Unsupported`** for this wire half (the
    // recorded `beam_dist` handoff in `src/dist.rs`: the external form is not an alias
    // key here), and emphatically *not* a delivery to whichever process the raw id
    // happens to name. The C's silent-drop half of the rule
    // (`erl_proc_sig_queue.c:6036-6046`: *"Not an alias … drop message"*) is pinned
    // where it IS reachable — for a **local** reference — by the sink-level case
    // `an_unowned_reference_is_dropped_and_a_non_ref_target_is_unsupported`.
    let name = rt
        .dist_node()
        .expect("a configured node")
        .node_name()
        .to_string();
    let creation = rt.dist_node().expect("a configured node").creation();
    let node_atom = rt.intern(&name);
    let to = make_external_ref(&mut heap, node_atom, &[0u32, 0u32, 1u32], creation)
        .expect("materialize the external reference");
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
        "a wire reference is the external form, which is not an alias key: \
         the honest outcome is `Unsupported`, never a delivery or a wrong drop: {:?}",
        dispatched[0].delivery
    );
    assert_eq!(only_message(&rt, rt.self_pid()), None);
}
#[test]
fn the_routed_link_and_exit_families_are_applied_through_the_wire() {
    let mut rt = node_runtime("signal");
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(HEAP_WORDS).expect("heap");

    let pid = rt.self_pid();
    // A local `to` as it arrives on the wire: an external pid whose node/creation are
    // this node's (`DOP_LINK`'s `To` may be either form).
    let to = own_pid_term(&mut rt, &mut heap);
    // A well-formed `From`: an external pid of **this link's peer**, whose node and
    // creation are exactly what the `'N'` handshake advertised.
    let peer_node = rt.intern(PEER_NODE_NAME);
    let peer_pid = make_external_pid(&mut heap, peer_node, 3, 0, CLIENT_CREATION)
        .expect("the peer's external pid");
    let peer = RemotePid::from_eterm(peer_pid).expect("decompose the peer pid");
    // `DOP_EXIT` addresses a **local** pid (`is_not_internal_pid(to)`,
    // `dist.c:2793-2797`). A real ERTS peer puts this node's **external** pid on the
    // wire and the receiving C decoder demotes it to an internal one; the harness
    // decoder installs no local-node identity, so the external form is what the sink
    // sees — and it resolves by node + creation + slot exactly like the internal form
    // (`LocalDeliverySink::resolve`).
    let local = own_pid_term(&mut rt, &mut heap);

    // 1. `DOP_LINK`: the remote `from` is now linked to the local `to`, recorded
    // **without** echoing a `DOP_LINK` back (the C's `break; /* Done */`, `dist.c:2258`).
    client
        .send_control(Dop::Link, &[peer_pid, to], rt.code_server_atoms_mut())
        .expect("queue the link");
    relay(&mut client, &mut server);
    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch the link");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::Delivered,
        "a link is routed, not refused: {:?}",
        dispatched[0].delivery
    );
    assert_eq!(
        rt.processes().remote_links(pid),
        [peer].as_slice(),
        "the accepted link records the remote partner against the local process"
    );

    // 2. `DOP_EXIT` from that recorded link: the local link-exit family removes the
    // record first and then applies the exit policy. Reason `normal` to a
    // non-trapping receiver is *ignored* — the signal is applied, never enqueued as a
    // message, and the recorded link is gone.
    let normal = rt.intern("normal");
    client
        .send_control(
            Dop::Exit,
            &[peer_pid, local, normal],
            rt.code_server_atoms_mut(),
        )
        .expect("queue the exit");
    relay(&mut client, &mut server);
    let dispatched = rt
        .dispatch_inbound(&mut heap, &mut server)
        .expect("dispatch the exit");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].outcome,
        DeliveryOutcome::Delivered,
        "an exit signal is applied locally: {:?}",
        dispatched[0].delivery
    );
    assert!(
        rt.processes().remote_links(pid).is_empty(),
        "a link exit removes the recorded remote link"
    );
    assert_eq!(
        only_message(&rt, pid),
        None,
        "an exit signal is applied, never delivered as an ordinary message"
    );
    assert!(
        rt.processes().contains(pid),
        "a `normal` link exit to a non-trapping receiver must not terminate it"
    );
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
// (6b) Every inbound delivery is validated against **its own link**
// ---------------------------------------------------------------------------
//
// Both halves of the runtime's inbound drive — the listening node
// ([`crate::dist::DistNode::dispatch_inbound`]) and the connector
// ([`crate::dist_client::DistClient::dispatch_inbound`]) — drain through a driver's
// `dispatch_inbound_checked`, so a delivery is checked against **that** link's
// peer/own identity (the C's per-opcode `invalid_message` shape checks plus
// `dep != external_pid_dist_entry(from)`, `dist.c`) before it can reach a mailbox. A
// validation failure is a link-level `Err`: the link is **retired** and the reason
// recorded (`DistNode::last_link_error`) — the C's connection kill.
//
// The tests below prove the validated path is live on the runtime's *own* drive (a real
// loopback link driven by `Runtime::run_one_slice`): a malformed or forged control
// message is never delivered and retires the link, while a well-formed one on the same
// link is delivered normally.

/// The node name the tests' `ConnectionDriver::client` advertises, so it is the
/// **peer** name the runtime's server-side driver learns from the `'N'` handshake —
/// and therefore the node a well-formed inbound `From` must name. Must stay in step
/// with [`CLIENT_NAME`].
const PEER_NODE_NAME: &str = "client@host";

/// The cookie [`node_runtime`] starts the runtime with.
const LIVE_COOKIE: &[u8] = b"delivery-cookie";

/// Opens a **real loopback link** to `rt`'s own listener and drives it to
/// `Established`, returning the client side (socket, driver, scratch heap, atom table)
/// for the test to write inbound control messages on.
fn live_link(rt: &mut Runtime) -> (TcpStream, ConnectionDriver, Heap, AtomTable) {
    // One pass publishes the node and opens the listener for accepting.
    rt.run_one_slice().expect("a scheduler pass");
    let port = rt.dist_node().expect("a configured node").port();

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the listener");
    stream.set_nonblocking(true).expect("non-blocking socket");
    let mut client = ConnectionDriver::client(CLIENT_NAME, LIVE_COOKIE, CLIENT_CREATION);

    let deadline = Instant::now() + LIVE_TIMEOUT;
    loop {
        round(&mut stream, &mut client);
        rt.run_one_slice().expect("a scheduler pass");
        if client.state() == DriverState::Established {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the loopback link never reached Established within {LIVE_TIMEOUT:?}"
        );
        std::thread::sleep(POLL_SLEEP);
    }
    (
        stream,
        client,
        Heap::new(HEAP_WORDS).expect("client heap"),
        AtomTable::new(),
    )
}

/// Drives loopback rounds and runtime passes until `condition` holds, failing with
/// `what` after [`LIVE_TIMEOUT`].
fn drive_until<F: Fn(&Runtime) -> bool>(
    rt: &mut Runtime,
    stream: &mut TcpStream,
    client: &mut ConnectionDriver,
    what: &str,
    condition: F,
) {
    let deadline = Instant::now() + LIVE_TIMEOUT;
    loop {
        round(stream, client);
        rt.run_one_slice().expect("a scheduler pass");
        if condition(rt) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the runtime never {what} within {LIVE_TIMEOUT:?}"
        );
        std::thread::sleep(POLL_SLEEP);
    }
}

/// How many messages `pid`'s mailbox holds.
fn mailbox_len(rt: &Runtime, pid: ProcessId) -> usize {
    match rt.processes().get(pid) {
        Some(process) => process.mailbox().iter().count(),
        None => 0,
    }
}

/// A **malformed** inbound `DOP_REG_SEND` reaches no mailbox and **retires its link**,
/// while a well-formed one on the *same* link is delivered first.
///
/// The forged shape is `dist.c:2486`'s `is_not_pid(from)`: a sender that is not a pid
/// is an `invalid_message` — a connection kill, not a delivery. The **unchecked**
/// classifier would have delivered it (the sink resolves the *recipient*, which here is
/// a genuinely registered local name), so the mailbox count is exactly what proves the
/// validator ran.
#[test]
fn a_malformed_inbound_reg_send_is_refused_and_retires_its_link() {
    let mut rt = node_runtime("forged");
    rt.allow_inbound_delivery(true);
    let (mut stream, mut client, mut heap, mut atoms) = live_link(&mut rt);

    let peer_node = make_atom(atoms.intern(PEER_NODE_NAME));
    let pid = rt.self_pid();
    // The recipient is the runtime's own root process, which `Runtime::new` registers as
    // `init` — a real local name, so the recipient alone would never stop a delivery.
    let name = make_atom(atoms.intern("init"));
    let empty = make_small(0);
    // A well-formed sender: an external pid of **this link's peer**.
    let peer_pid =
        make_external_pid(&mut heap, peer_node, 3, 0, CLIENT_CREATION).expect("the peer's pid");
    let well_formed = make_small(4242);
    let forged = make_small(6666);

    // 1. Well-formed: `From` is a pid and the name is registered ⇒ delivered.
    client
        .send_control(Dop::RegSend, &[peer_pid, empty, name, well_formed], &atoms)
        .expect("queue the well-formed reg_send");
    drive_until(
        &mut rt,
        &mut stream,
        &mut client,
        "deliver the well-formed reg_send",
        |rt| only_message(rt, pid) == Some(well_formed),
    );
    assert_eq!(
        mailbox_len(&rt, pid),
        1,
        "exactly the well-formed message reached the mailbox"
    );
    {
        let node = rt.dist_node().expect("a configured node");
        assert!(
            !node.is_idle(),
            "the link that carried a valid message is still live"
        );
        assert!(
            node.last_link_error().is_none(),
            "a well-formed message retires nothing: {:?}",
            node.last_link_error()
        );
    }

    // 2. Forged: a sender that is not a pid (`is_not_pid(from)`, `dist.c:2486`) is an
    // `invalid_message`. It must not be delivered, and it must retire the link.
    client
        .send_control(Dop::RegSend, &[make_small(1), empty, name, forged], &atoms)
        .expect("queue the forged reg_send");
    drive_until(
        &mut rt,
        &mut stream,
        &mut client,
        "retire the link that carried a forged control message",
        |rt| rt.dist_node().expect("a configured node").is_idle(),
    );
    assert_eq!(
        mailbox_len(&rt, pid),
        1,
        "a malformed control message must never reach a mailbox"
    );
    assert_eq!(
        only_message(&rt, pid),
        Some(well_formed),
        "the mailbox still holds only the well-formed message"
    );
    let error = rt
        .dist_node()
        .expect("a configured node")
        .last_link_error()
        .expect("the reason the forged link was retired is recorded")
        .to_string();
    assert!(
        error.contains(PEER_NODE_NAME),
        "the retirement names the peer it retired: {error}"
    );
}

/// The **per-link identity** check (`dep != external_pid_dist_entry(from)`,
/// `dist.c:2237`) is live: the very same `DOP_LINK` opcode is accepted from the peer's
/// own node — the link keeps working afterwards — and refused from a **third** node's,
/// which retires that link.
#[test]
fn a_forged_inbound_link_from_a_third_node_retires_its_link() {
    let mut rt = node_runtime("spoofed");
    rt.allow_inbound_delivery(true);
    let (mut stream, mut client, mut heap, mut atoms) = live_link(&mut rt);

    let node_name = rt
        .dist_node()
        .expect("a configured node")
        .node_name()
        .to_string();
    let creation = rt.dist_node().expect("a configured node").creation();
    let our_node = make_atom(atoms.intern(&node_name));
    let peer_node = make_atom(atoms.intern(PEER_NODE_NAME));
    let other_node = make_atom(atoms.intern("somewhere-else@host"));
    let pid = rt.self_pid();
    let to = make_external_pid(
        &mut heap,
        our_node,
        pid.number() as u32,
        pid.serial() as u32,
        creation,
    )
    .expect("the addressed pid");
    let name = make_atom(atoms.intern("init"));
    let empty = make_small(0);
    let peer_pid =
        make_external_pid(&mut heap, peer_node, 3, 0, CLIENT_CREATION).expect("the peer's pid");
    let third_pid = make_external_pid(&mut heap, other_node, 3, 0, CLIENT_CREATION)
        .expect("a third node's pid");

    // 1. `From` names this link's **peer** ⇒ well-formed. The accepted `DOP_LINK`
    // **records** the remote link (route-free, echoing nothing), so the observable is
    // that the link survives: the message queued *behind* the `DOP_LINK` is delivered,
    // and the driver drains in arrival order, so the link was accepted before that
    // delivery happened.
    let accepted = make_small(1010);
    client
        .send_control(Dop::Link, &[peer_pid, to], &atoms)
        .expect("queue the peer's own link");
    client
        .send_control(Dop::RegSend, &[peer_pid, empty, name, accepted], &atoms)
        .expect("queue the message behind the link");
    drive_until(
        &mut rt,
        &mut stream,
        &mut client,
        "accept a DOP_LINK authored by the peer and keep the link",
        |rt| only_message(rt, pid) == Some(accepted),
    );
    {
        let node = rt.dist_node().expect("a configured node");
        assert!(
            !node.is_idle(),
            "a DOP_LINK from the link's own peer must not retire it"
        );
        assert!(node.last_link_error().is_none());
    }

    // 2. The same opcode with a `From` on a **third** node is the spoof `dist.c:2237`
    // rejects. Nothing it was queued with may be delivered, and the link is retired.
    let lost = make_small(2020);
    client
        .send_control(Dop::Link, &[third_pid, to], &atoms)
        .expect("queue the third node's link");
    client
        .send_control(Dop::RegSend, &[peer_pid, empty, name, lost], &atoms)
        .expect("queue the message behind the spoofed link");
    drive_until(
        &mut rt,
        &mut stream,
        &mut client,
        "retire the link that carried a DOP_LINK from a third node",
        |rt| rt.dist_node().expect("a configured node").is_idle(),
    );
    assert_eq!(
        mailbox_len(&rt, pid),
        1,
        "nothing queued behind the spoofed control message may be delivered"
    );
    assert_eq!(
        only_message(&rt, pid),
        Some(accepted),
        "the mailbox still holds only the message the accepted link delivered"
    );
    let error = rt
        .dist_node()
        .expect("a configured node")
        .last_link_error()
        .expect("the reason the spoofed link was retired is recorded")
        .to_string();
    assert!(
        error.contains(PEER_NODE_NAME),
        "the retirement names the link's peer, not the spoofed node: {error}"
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

/// An external pid on the fixture's [`FOREIGN_NODE_NAME`] node — a remote peer that is
/// never a process of this fixture's table.
fn foreign_pid(fixture: &Fixture, heap: &mut Heap, number: u64) -> Eterm {
    external_pid_term(
        heap,
        make_atom(fixture.foreign_node),
        number,
        0,
        CLIENT_CREATION,
    )
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

    // The `From` of a `DOP_SEND_SENDER`: a boxed external pid on *this* node. The
    // payload is a **compound** term, so "still readable afterwards" is a real
    // structural check rather than a trivially-immediate one.
    let from = external_pid_term(&mut heap, make_atom(node), 9, 3, NODE_CREATION);
    let payload = heap
        .tuple(&[make_small(11), make_small(22)])
        .expect("build the payload tuple");
    let delivery = Delivery::Send {
        from,
        to: pid.to_eterm(),
        message: payload,
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );

    let delivered = mailbox(&fixture.table, pid);
    assert_eq!(delivered.len(), 1, "exactly one message was delivered");
    assert_eq!(
        size_object(delivered[0].0),
        3,
        "the mailbox must hold the whole `{{11,22}}` payload tuple, not a bare pointer"
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
    let rebuilt = heap
        .tuple(&[make_small(11), make_small(22)])
        .expect("rebuild the payload tuple");
    assert_eq!(size_object(after[0].0), 3, "the payload must still be walkable");
    assert!(
        eq(after[0].0, rebuilt, &fixture.atoms),
        "the delivered payload must still compare `=:=` to the tuple that was sent"
    );
    assert!(is_immediate(after[0].1));
    assert_eq!(after[0].1, make_atom(node));
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

// --- the routed signal families (route-free receipt) ------------------------
//
// The inbound signal families below do **not** go through a *locally initiated*
// `beam_process` entry point (`link_term`/`unlink_term`/`monitor_term`/`demonitor`/
// `send_exit_term`), each of which would route a `DOP_*` the C's receipt arm never
// sends. Instead they install (or drop) their local record through a **route-free
// receipt** primitive and echo nothing — the C's `break; /* Done */` (`dist.c:2258`).

/// An inbound `DOP_LINK` **records** the remote partner against the local process
/// (`dist.c:2224`), echoing no `DOP_LINK` back and delivering no message.
#[test]
fn an_inbound_link_records_a_remote_link() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let from = foreign_pid(&fixture, &mut heap, 3);
    let remote = RemotePid::from_eterm(from).expect("decompose the remote pid");

    let delivery = Delivery::Link {
        from,
        to: pid.to_eterm(),
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered,
        "an inbound link is accepted"
    );
    assert_eq!(
        fixture.table.remote_links(pid),
        [remote].as_slice(),
        "the accepted link records the remote partner against the local process"
    );
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "a link installs a relationship; it delivers no message"
    );
}

/// An exit signal is **applied** to the local process and never enqueued as an
/// ordinary message. The link-exit family (`DOP_EXIT`) ignores an exit with no
/// recorded link; the untrappable `erlang:exit/2` family (`DOP_EXIT2`) consults no
/// link and terminates a non-trapping target.
#[test]
fn an_inbound_exit_signal_is_applied_not_delivered_as_a_message() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let from = foreign_pid(&fixture, &mut heap, 3);

    // A link exit with **no** matching recorded link is ignored outright
    // (`erl_proc_sig_queue.c:4674-4680`) — never enqueued, never fatal.
    let unmatched = Delivery::Exit {
        from,
        to: pid.to_eterm(),
        reason: make_small(7),
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &unmatched),
        DeliveryOutcome::Delivered,
        "an unmatched link exit is applied (as a no-op), not refused"
    );
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "an exit signal must never become an ordinary message"
    );
    assert!(
        fixture.table.contains(pid),
        "an unmatched link exit is ignored, not fatal"
    );

    // The untrappable family consults no link: it is applied to the target, and a
    // non-trapping one really terminates — still without a message.
    let victim = fixture.spawn();
    let explicit = Delivery::Exit2 {
        from,
        to: victim.to_eterm(),
        reason: make_small(7),
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &explicit),
        DeliveryOutcome::Delivered,
        "the untrappable family is applied, not refused"
    );
    assert!(
        !fixture.table.contains(victim),
        "an untrappable non-normal exit terminates a non-trapping target"
    );
}

/// An out-of-contract signal is refused rather than substituted: a `From` that is not
/// a remote pid is `Unsupported`, and a `To` on another node is never resolved by its
/// slot number alone (`NoSuchProcess`).
#[test]
fn an_out_of_contract_signal_is_unsupported_not_substituted() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");

    let non_remote = Delivery::Exit {
        from: make_small(1),
        to: pid.to_eterm(),
        reason: make_small(3),
        token: None,
    };
    assert_eq!(
        route(&mut fixture.table, node, &non_remote),
        DeliveryOutcome::Unsupported,
        "a `From` that is not a remote pid is outside the signal opcodes' contract"
    );

    let from = foreign_pid(&fixture, &mut heap, 3);
    let elsewhere = foreign_pid(&fixture, &mut heap, 9);
    let foreign_target = Delivery::Link {
        from,
        to: elsewhere,
    };
    assert_eq!(
        route(&mut fixture.table, node, &foreign_target),
        DeliveryOutcome::NoSuchProcess,
        "a `To` on another node is not a process of this runtime"
    );
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "a refused signal must not reach any mailbox"
    );
}

/// The OTP-26+ unlink-id handshake: `id == 0` owes no ack and is `Delivered` (the link
/// record is dropped); `id != 0` drops the record too but owes a `DOP_UNLINK_ID_ACK` the
/// sink cannot emit, so it reports the wire half `Unsupported`; the `_ACK` form consumes
/// an ack and is `Delivered`.
#[test]
fn an_inbound_unlink_id_without_an_ack_is_delivered_and_one_owing_an_ack_is_unsupported() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let from = foreign_pid(&fixture, &mut heap, 3);
    let link = Delivery::Link {
        from,
        to: pid.to_eterm(),
    };

    // `Id == 0`: the C's `reply_dist_unlink_ack` sends nothing (`erl_proc_sig_queue.c:2753-2760`),
    // so the unlink completes locally and is `Delivered`; the record is dropped.
    assert_eq!(
        route(&mut fixture.table, node, &link),
        DeliveryOutcome::Delivered
    );
    let no_ack = Delivery::UnlinkId {
        id: make_small(0),
        from,
        to: pid.to_eterm(),
    };
    assert_eq!(
        route(&mut fixture.table, node, &no_ack),
        DeliveryOutcome::Delivered,
        "an unlink that owes no ack completes locally"
    );
    assert!(
        fixture.table.remote_links(pid).is_empty(),
        "the `id == 0` unlink drops the recorded link"
    );

    // `Id != 0`: the record is still dropped, but the owed ack has no channel.
    assert_eq!(
        route(&mut fixture.table, node, &link),
        DeliveryOutcome::Delivered
    );
    let owing_ack = Delivery::UnlinkId {
        id: make_small(9),
        from,
        to: pid.to_eterm(),
    };
    assert_eq!(
        route(&mut fixture.table, node, &owing_ack),
        DeliveryOutcome::Unsupported,
        "the owed DOP_UNLINK_ID_ACK is the wire half this sink cannot emit"
    );
    assert!(
        fixture.table.remote_links(pid).is_empty(),
        "the link record is dropped even when the owed ack is unemittable"
    );

    // The `_ACK` form consumes an ack: it owes no reply and is `Delivered` for a live
    // local `to`.
    let ack = Delivery::UnlinkIdAck {
        id: make_small(9),
        from,
        to: pid.to_eterm(),
    };
    assert_eq!(
        route(&mut fixture.table, node, &ack),
        DeliveryOutcome::Delivered,
        "consuming an ack owes nothing back"
    );
    assert!(mailbox(&fixture.table, pid).is_empty());
}

/// A `DOP_MONITOR_P_EXIT` fires exactly when its reference is one **this** node minted for
/// an outgoing remote monitor (the peer echoes that reference image back verbatim); a DOWN
/// for any other reference is the C's silent drop.
#[test]
fn a_monitor_down_for_our_own_outgoing_reference_fires_or_is_dropped() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let watched = foreign_pid(&fixture, &mut heap, 3);

    // An outgoing remote monitor this node initiated: the reference is what the peer
    // echoes back in the `DOP_MONITOR_P_EXIT` it sends when the target dies.
    let monitor = fixture
        .table
        .monitor_term(pid, watched, MonitorKind::Process)
        .expect("record the outgoing remote monitor");
    let id = monitor.id();

    // The wire image of **our own** reference, exactly as `dist_client` packs it onto
    // the outgoing `DOP_MONITOR_P`: `{our_node, our_creation, [0, id >> 32, id]}`.
    let echoed = make_external_ref(
        &mut heap,
        make_atom(node),
        &[0u32, (id >> 32) as u32, id as u32],
        NODE_CREATION,
    )
    .expect("the echoed reference image");
    let reason = make_small(7);
    let down = Delivery::MonitorExit {
        watched,
        watcher: pid.to_eterm(),
        reference: echoed,
        reason,
    };
    assert_eq!(
        route(&mut fixture.table, node, &down),
        DeliveryOutcome::Delivered,
        "a DOWN for our own outgoing record fires"
    );
    assert_eq!(
        mailbox(&fixture.table, pid).len(),
        1,
        "the DOWN is delivered into the watcher's mailbox"
    );

    // The delivered message is the oracle's `{'DOWN', Ref, process, RemotePid, Reason}`.
    let delivered = mailbox(&fixture.table, pid);
    let down_atom = make_atom(fixture.atoms.intern("DOWN"));
    let process_atom = make_atom(fixture.atoms.intern("process"));
    let mut scratch = Heap::new(HEAP_WORDS).expect("scratch heap");
    let expected_reference = monitor.to_eterm(&mut scratch).expect("the DOWN's reference");
    let expected_target = foreign_pid(&fixture, &mut scratch, 3);
    let expected = scratch
        .tuple(&[down_atom, expected_reference, process_atom, expected_target, reason])
        .expect("build the expected DOWN");
    assert!(
        eq(delivered[0].0, expected, &fixture.atoms),
        "the DOWN is `{{'DOWN', Ref, process, RemotePid, Reason}}`"
    );

    // A reference this node never minted as a monitor matches no record: the C resolves
    // the DOWN through the watcher's monitor tree and **drops** a miss.
    let stray = make_external_ref(
        &mut heap,
        make_atom(node),
        &[0u32, 0u32, id.wrapping_add(1000) as u32],
        NODE_CREATION,
    )
    .expect("a reference with no matching record");
    let unmatched = Delivery::MonitorExit {
        watched,
        watcher: pid.to_eterm(),
        reference: stray,
        reason,
    };
    assert_eq!(
        route(&mut fixture.table, node, &unmatched),
        DeliveryOutcome::Dropped,
        "a DOWN matching no outgoing record is silently dropped"
    );
    assert_eq!(
        mailbox(&fixture.table, pid).len(),
        1,
        "a dropped DOWN adds no second message"
    );
}

/// A remote watcher's own monitor is still unrouted: the receipt `RemoteWatcher` is keyed
/// by a bare `u64` that a peer-minted wire reference has no faithful image of, so both the
/// monitor and its demonitor are the honest `Unsupported` and record nothing.
#[test]
fn an_inbound_monitor_or_demonitor_is_the_honest_unsupported() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let watcher = foreign_pid(&fixture, &mut heap, 3);
    let reference = make_external_ref(
        &mut heap,
        make_atom(fixture.foreign_node),
        &[1u32, 2u32, 3u32],
        CLIENT_CREATION,
    )
    .expect("a peer-minted reference image");

    let monitor = Delivery::Monitor {
        watcher,
        watched: pid.to_eterm(),
        reference,
    };
    assert_eq!(
        route(&mut fixture.table, node, &monitor),
        DeliveryOutcome::Unsupported,
        "a peer-minted monitor reference has no faithful local key"
    );
    assert!(
        fixture.table.remote_watchers(pid).is_empty(),
        "an unsupported monitor must record nothing"
    );

    let demonitor = Delivery::Demonitor {
        watcher,
        watched: pid.to_eterm(),
        reference,
    };
    assert_eq!(
        route(&mut fixture.table, node, &demonitor),
        DeliveryOutcome::Unsupported
    );
    assert!(mailbox(&fixture.table, pid).is_empty());
}

/// An inbound `DOP_GROUP_LEADER` installs the remote `from` as the local `to`'s group
/// leader — readable back through [`ProcessTable::group_leader`] — and routes nothing.
#[test]
fn an_inbound_group_leader_sets_the_leader() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let leader = foreign_pid(&fixture, &mut heap, 3);
    let remote = RemotePid::from_eterm(leader).expect("decompose the leader pid");

    let delivery = Delivery::GroupLeader {
        from: leader,
        to: pid.to_eterm(),
    };
    assert_eq!(
        route(&mut fixture.table, node, &delivery),
        DeliveryOutcome::Delivered
    );
    assert_eq!(
        fixture.table.group_leader(pid),
        Some(PidTarget::Remote(remote)),
        "the remote sender becomes the local process's group leader"
    );
    assert!(
        mailbox(&fixture.table, pid).is_empty(),
        "installing a group leader delivers no message"
    );
}

/// A distribution seam that records every routed signal instead of touching a socket.
///
/// The log is shared through an `Rc<RefCell<_>>` because the seam lives behind a
/// `Box<dyn DistSignalRouter>` inside the table and is not downcastable — the test keeps
/// its own handle to inspect what was routed (and, crucially, to prove **nothing** was).
struct RecordingRouter {
    signals: Rc<RefCell<Vec<DistSignal>>>,
}

impl DistSignalRouter for RecordingRouter {
    fn route(&mut self, signal: DistSignal) -> BeamResult<()> {
        self.signals.borrow_mut().push(signal);
        Ok(())
    }
}

/// **The route-free proof.** Every family the sink now routes is driven in turn with a
/// recording distribution seam installed: an inbound receipt installs/drops its local
/// record and must never echo a `DOP_*` back on the wire (the C's `break; /* Done */`),
/// so the recorded signal vector must stay **empty**.
#[test]
fn every_routed_inbound_family_routes_nothing_outbound() {
    let mut fixture = Fixture::new();
    let node = fixture.local_node;
    let pid = fixture.pid;
    let mut heap = Heap::new(HEAP_WORDS).expect("scratch heap");
    let peer = foreign_pid(&fixture, &mut heap, 3);
    let normal = make_atom(fixture.atoms.intern("normal"));

    // The outgoing remote monitor the DOWN resolves — set up **before** the recorder is
    // installed, because establishing it IS a locally initiated signal that must
    // legitimately route (the receipt families below must not).
    let monitor = fixture
        .table
        .monitor_term(pid, peer, MonitorKind::Process)
        .expect("record the outgoing remote monitor");
    let id = monitor.id();
    let echoed = make_external_ref(
        &mut heap,
        make_atom(node),
        &[0u32, (id >> 32) as u32, id as u32],
        NODE_CREATION,
    )
    .expect("the echoed reference image");

    let log = Rc::new(RefCell::new(Vec::new()));
    fixture.table.set_dist_router(Box::new(RecordingRouter {
        signals: Rc::clone(&log),
    }));

    let directed = pid.to_eterm();
    let families = [
        Delivery::Link {
            from: peer,
            to: directed,
        },
        Delivery::Exit {
            from: peer,
            to: directed,
            reason: normal,
            token: None,
        },
        Delivery::Link {
            from: peer,
            to: directed,
        },
        Delivery::UnlinkId {
            id: make_small(0),
            from: peer,
            to: directed,
        },
        Delivery::UnlinkIdAck {
            id: make_small(9),
            from: peer,
            to: directed,
        },
        Delivery::Exit2 {
            from: peer,
            to: directed,
            reason: normal,
            token: None,
        },
        Delivery::MonitorExit {
            watched: peer,
            watcher: directed,
            reference: echoed,
            reason: make_small(7),
        },
        Delivery::GroupLeader {
            from: peer,
            to: directed,
        },
    ];
    for delivery in &families {
        assert_eq!(
            route(&mut fixture.table, node, delivery),
            DeliveryOutcome::Delivered,
            "every routed family really was applied: {delivery:?}"
        );
    }
    assert!(
        log.borrow().is_empty(),
        "a route-free receipt must never echo a `DOP_*`: {:?}",
        log.borrow()
    );
}

// --- the escalation list ----------------------------------------------------

/// **One row per remaining unrouted family**: after the link/exit/unlink/monitor-down/
/// group-leader receipt families were routed, the families `beam_process` still has no
/// route-free counterpart for are the honest `Unsupported` — never a substituted
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

    let rows: [(Dop, Delivery); 5] = [
        // A remote watcher: a monitor's origin side is a local pid here, and the receipt
        // `RemoteWatcher` is keyed by a bare `u64` `MonitorRef` that a peer-minted wire
        // reference has no faithful image of (a lossy pack could collapse two remote
        // monitors into one record).
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
