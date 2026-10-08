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
//! Small integers are used as the payload wherever the exact term matters: unlike
//! atoms (whose raw encoding is table-local), a small integer is its own immediate
//! value, so a term compares `==` across heaps.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::make_external_pid;
use beam_dist::flags::Dop;
use beam_dist::{ConnectionDriver, DeliveryOutcome, DriverState, StepOutcome};
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::prelude::{make_atom, make_small, Eterm};
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
fn only_message(rt: &Runtime, pid: beam_process::ProcessId) -> Option<Eterm> {
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

    // An alias cannot be resolved here (this runtime has no alias table), so the
    // delivery must be refused rather than delivered to whichever pid the *number*
    // happens to name.
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
// (5) The scheduler-pass gate: routing is off unless it is asked for
// ---------------------------------------------------------------------------

#[test]
fn inbound_routing_is_off_by_default_and_toggleable() {
    let mut rt = node_runtime("gate");
    assert!(
        !rt.inbound_delivery_enabled(),
        "an inbound delivery must never be routed into a runtime that did not ask for it"
    );
    rt.allow_inbound_delivery(true);
    assert!(rt.inbound_delivery_enabled());
    rt.allow_inbound_delivery(false);
    assert!(!rt.inbound_delivery_enabled());
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
