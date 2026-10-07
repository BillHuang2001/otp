#![deny(unsafe_code)]

//! Hermetic tests for the inbound `DOP_*` **delivery** seam
//! ([`beam_dist::delivery`]): a decoded control message is classified into a
//! semantic [`Delivery`] and routed through an [`InboundSink`].
//!
//! No sockets are involved: a **client** and a **server** [`ConnectionDriver`]
//! are wired to each other and their [`ConnectionDriver::take_output`] bytes are
//! shuttled by hand (the same harness as `tests/driver.rs`). Every expectation is
//! the *observable* classification of a real wire message built with
//! [`ConnectionDriver::send_control`] / [`ConnectionDriver::send_message`] — never
//! an internal detail.
//!
//! Small integers are used as the control/message terms wherever the exact terms
//! matter: unlike atoms (whose raw encoding is table-local), a small integer is
//! its own immediate value, so a term compares `==` across the two drivers' heaps.
//! The tests therefore pin both the *variant* (which argument is which) and the
//! *exact* terms.

use beam_alloc::Heap;
use beam_common::Result;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::flags::Dop;
use beam_dist::{Delivery, DeliveryOutcome, Dispatched, InboundSink, UnsupportedSink};
use beam_term::prelude::{make_small, Eterm};
use beam_term::AtomTable;

/// The shared cookie both ends authenticate with.
const COOKIE: &[u8] = b"s3cr3t-cookie";
/// The connecting node's name.
const CLIENT_NAME: &[u8] = b"client@host";
/// The accepting node's name.
const SERVER_NAME: &[u8] = b"server@host";
/// The connecting node's creation.
const CLIENT_CREATION: u32 = 7;
/// The accepting node's creation.
const SERVER_CREATION: u32 = 0xdead_beef;

/// Drives a client and a server driver against each other until both are
/// established (the `tests/driver.rs` harness, whole-buffer mode).
fn establish() -> (ConnectionDriver, ConnectionDriver) {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
    let mut server = ConnectionDriver::server(SERVER_NAME, COOKIE, SERVER_CREATION);

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
            panic!("driver failed: client={:?}, server={:?}", client.state(), server.state());
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

/// Decodes and classifies the next inbound delivery on `server`.
fn next_delivery(
    server: &mut ConnectionDriver,
    heap: &mut Heap,
    atoms: &mut AtomTable,
) -> Delivery {
    server
        .next_delivery(heap, atoms)
        .expect("decode inbound delivery")
        .expect("an inbound delivery")
}

/// An [`InboundSink`] that records every delivery it receives and reports
/// [`DeliveryOutcome::Delivered`].
#[derive(Default)]
struct RecordingSink {
    deliveries: Vec<Delivery>,
}

impl InboundSink for RecordingSink {
    fn deliver(&mut self, delivery: &Delivery) -> Result<DeliveryOutcome> {
        self.deliveries.push(delivery.clone());
        Ok(DeliveryOutcome::Delivered)
    }
}

// ---------------------------------------------------------------------------
// (a) A `send` message is classified with exactly the terms sent, and a sink
//     receives it with the outcome the sink returned.
// ---------------------------------------------------------------------------

#[test]
fn send_sender_classifies_and_a_sink_receives_it() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(100);
    let to = make_small(200);
    let message = make_small(42);

    // `send_message` selects `DOP_SEND_SENDER` because the negotiated set includes
    // `DFLAG_SEND_SENDER`; the trailing message carries the payload.
    client.send_message(from, to, message, &atoms).expect("send message");
    relay(&mut client, &mut server);

    // --- the classification view -------------------------------------------
    let delivery = next_delivery(&mut server, &mut heap, &mut atoms);
    assert_eq!(
        delivery,
        Delivery::Send { from, to, message, token: None },
        "a DOP_SEND_SENDER classifies as Delivery::Send with exactly the terms sent",
    );
    assert_eq!(delivery.dop(), Dop::Send);

    // --- the routed view: an InboundSink records it and its outcome is kept --
    client.send_message(from, to, message, &atoms).expect("send message 2");
    relay(&mut client, &mut server);

    let mut sink = RecordingSink::default();
    let dispatched = server.dispatch_inbound(&mut heap, &mut atoms, &mut sink).expect("dispatch");
    assert_eq!(
        dispatched,
        vec![Dispatched {
            delivery: Delivery::Send { from, to, message, token: None },
            outcome: DeliveryOutcome::Delivered,
        }],
        "dispatch_inbound returns the classified delivery and the sink's outcome",
    );
    assert_eq!(
        sink.deliveries,
        vec![Delivery::Send { from, to, message, token: None }],
        "the sink observed the delivery",
    );
}

#[test]
fn alias_send_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    let message = make_small(3);
    client.send_control(Dop::AliasSend, &[from, to, message], &atoms).expect("send");
    relay(&mut client, &mut server);

    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::AliasSend { from, to, message, token: None },
    );
}

#[test]
fn reg_send_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(11);
    let cookie = make_small(22);
    let name = make_small(33);
    let message = make_small(44);
    client
        .send_control(Dop::RegSend, &[from, cookie, name, message], &atoms)
        .expect("send");
    relay(&mut client, &mut server);

    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::RegSend { from, cookie, name, message, token: None },
    );
}

#[test]
fn link_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(5);
    let to = make_small(6);
    client.send_control(Dop::Link, &[from, to], &atoms).expect("send");
    relay(&mut client, &mut server);

    assert_eq!(next_delivery(&mut server, &mut heap, &mut atoms), Delivery::Link { from, to });
}

// ---------------------------------------------------------------------------
// (b) The exit family: `DOP_EXIT` and `DOP_EXIT_TT` both classify as
//     `Delivery::Exit`, the `_TT` form landing the token and reason correctly.
// ---------------------------------------------------------------------------

#[test]
fn exit_and_exit_tt_classify() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(7);
    let to = make_small(8);
    let reason = make_small(9);

    // `{DOP_EXIT, From, To, Reason}` — no trace token.
    client.send_control(Dop::Exit, &[from, to, reason], &atoms).expect("exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Exit { from, to, reason, token: None },
    );

    // `{DOP_EXIT_TT, From, To, TraceToken, Reason}` — the token and reason are
    // the two distinct trailing arguments, in that order (`dist.c:2773-2779`).
    let token = make_small(101);
    let tt_reason = make_small(202);
    client
        .send_control(Dop::ExitTt, &[from, to, token, tt_reason], &atoms)
        .expect("exit_tt");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Exit { from, to, reason: tt_reason, token: Some(token) },
        "the ExitTt token/reason must land in the token/reason fields respectively",
    );
}

// ---------------------------------------------------------------------------
// (c) The monitor family.
// ---------------------------------------------------------------------------

#[test]
fn monitor_family_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let watcher = make_small(1);
    let watched = make_small(2);
    let reference = make_small(3);

    client
        .send_control(Dop::MonitorP, &[watcher, watched, reference], &atoms)
        .expect("monitor");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Monitor { watcher, watched, reference },
    );

    client
        .send_control(Dop::DemonitorP, &[watcher, watched, reference], &atoms)
        .expect("demonitor");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Demonitor { watcher, watched, reference },
    );

    let reason = make_small(4);
    client
        .send_control(Dop::MonitorPExit, &[watched, watcher, reference, reason], &atoms)
        .expect("monitor_exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::MonitorExit { watched, watcher, reference, reason },
    );
}

// ---------------------------------------------------------------------------
// (d) The unlink-id family and the group leader.
// ---------------------------------------------------------------------------

#[test]
fn unlink_id_family_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let id = make_small(7);
    let from = make_small(8);
    let to = make_small(9);

    client.send_control(Dop::UnlinkId, &[id, from, to], &atoms).expect("unlink_id");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::UnlinkId { id, from, to },
    );

    client.send_control(Dop::UnlinkIdAck, &[id, from, to], &atoms).expect("unlink_id_ack");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::UnlinkIdAck { id, from, to },
    );
}

#[test]
fn group_leader_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    client.send_control(Dop::GroupLeader, &[from, to], &atoms).expect("group_leader");
    relay(&mut client, &mut server);

    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::GroupLeader { from, to },
    );
}

// ---------------------------------------------------------------------------
// (e) Recognised-but-unclassified opcodes are `Delivery::Other(dop)`, never a
//     substituted send.
// ---------------------------------------------------------------------------

#[test]
fn unclassified_opcodes_classify_as_other() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts}` — the distributed-spawn
    // family (`dist.c:2919`), which this crate recognises but does not classify.
    let terms: Vec<Eterm> = (1..=5).map(make_small).collect();
    client.send_control(Dop::SpawnRequest, &terms, &atoms).expect("spawn_request");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Other(Dop::SpawnRequest),
    );

    // `{DOP_PAYLOAD_EXIT, From, To}` — a payload exit (`dist.c:2757`). Because
    // `control.rs::carries_message` does NOT list the payload-exit forms, the
    // trailing payload term a `DFLAG_EXIT_PAYLOAD` peer would send cannot be
    // decoded here at all (the control-only form is all that decodes); what this
    // pins is the *classification* of that control-only form.
    client
        .send_control(Dop::PayloadExit, &[make_small(1), make_small(2)], &atoms)
        .expect("payload_exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Other(Dop::PayloadExit),
    );
}

// ---------------------------------------------------------------------------
// (f) The default trait method is honest: a sink with no runtime reports
//     `Unsupported`, never a wrong result.
// ---------------------------------------------------------------------------

#[test]
fn unsupported_sink_reports_unsupported() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    let message = make_small(3);
    client.send_message(from, to, message, &atoms).expect("send");
    relay(&mut client, &mut server);

    let mut sink = UnsupportedSink;
    let dispatched = server.dispatch_inbound(&mut heap, &mut atoms, &mut sink).expect("dispatch");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0],
        Dispatched {
            delivery: Delivery::Send { from, to, message, token: None },
            outcome: DeliveryOutcome::Unsupported,
        },
        "the default InboundSink reports Unsupported, proving it never mis-delivers",
    );
}
