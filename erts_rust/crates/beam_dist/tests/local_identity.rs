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

//! The **inbound decode-side local-identity seam** (contract A.5): a decoded
//! external pid/port/reference whose node is *this* node must become the
//! **internal** term, so `Pid =:= self()` holds for a pid a peer reflects back and
//! an inbound alias reference becomes a reference the runtime can resolve.
//!
//! [`LocalIdentity`] + [`decode_control_with_identity`] are the value-layer seam;
//! [`Connection::set_local_identity`] / [`ConnectionDriver::set_local_identity`] are
//! the per-link installation. Everything here is hermetic: control bodies are built
//! **byte by byte** (so each ETF wire tag is pinned explicitly) and decoded against
//! a `beam_alloc` heap + `AtomTable`; the two end-to-end legs use two endpoints of
//! this crate (a real loopback `TcpStream` pair and two hand-shuttled
//! [`ConnectionDriver`]s). No `erl`, no `epmd`, no `beam_rt`.
//!
//! Ground truth: `erts/emulator/beam/external.c` `dec_is_this_node` (`:3123-3129`),
//! `dec_pid` (`:3142-3197`), `dec_port` (`:4685-4721`), `ref_ext_common`
//! (`:4757-4862`), and `dist.c:2701-2740` (the alias/payload families).
//!
//! Every demotion case is also asserted **without** an identity installed — the
//! additivity proof that the seam cannot change what a caller without node identity
//! (today `beam_rt`) observes.

use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::term::{external_pid_parts, external_port_parts, external_ref_parts};
use beam_codec::{
    internal_ref_id, NEWER_REFERENCE_EXT, NEW_PID_EXT, NEW_PORT_EXT, PID_EXT, PORT_EXT,
    SMALL_ATOM_UTF8_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, V4_PORT_EXT,
};
use beam_common::BeamError;
use beam_dist::control::{decode_control, decode_control_with_identity};
use beam_dist::flags::Dop;
use beam_dist::{
    Connection, ConnectionDriver, ControlMessage, Delivery, DriverState, LinkIdentity,
    LocalIdentity, TcpListener, TcpTransport,
};
use beam_term::prelude::{
    internal_port_data, is_internal_pid, is_internal_port, make_internal_pid, make_pid_data,
    pid_number, pid_serial, Eterm,
};
use beam_term::AtomTable;

/// The node under test: the identity a receiving link installs.
const LOCAL_NODE: &str = "alpha@localhost";
/// Its creation. `<= 3` so the tiny-creation `PID_EXT`/`PORT_EXT` wire tags can
/// carry it too (`is_tiny_creation`, `external.c:71-72`).
const LOCAL_CREATION: u32 = 2;
/// A **different** node — the peer, and the decoy the negative cases use.
const OTHER_NODE: &str = "beta@localhost";
/// The peer's creation.
const OTHER_CREATION: u32 = 1;
/// The shared cookie for the live-handshake legs.
const COOKIE: &[u8] = b"local-identity-cookie";
/// A finite read timeout, so a stalled or broken link fails instead of hanging.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Wire builders — every ETF tag spelled out, so the tag under test is explicit.
// ---------------------------------------------------------------------------

/// `[SMALL_ATOM_UTF8_EXT][len:1][utf8]` — an inline atom (`external.c:dec_atom`).
fn atom_bytes(name: &str) -> Vec<u8> {
    let bytes = name.as_bytes();
    assert!(bytes.len() < 256, "a SMALL_ATOM_UTF8_EXT name is one length byte");
    let mut out = vec![SMALL_ATOM_UTF8_EXT, bytes.len() as u8];
    out.extend_from_slice(bytes);
    out
}

/// `[PID_EXT][node][number:4][serial:4][creation:1]` (`external.c:3150-3164`) —
/// the tiny-creation form. **The wire order is `number` then `serial`**; the
/// internal `make_pid_data(serial, number)` takes them the other way round.
fn pid_ext(node: &str, number: u32, serial: u32, creation: u8) -> Vec<u8> {
    let mut out = vec![PID_EXT];
    out.extend_from_slice(&atom_bytes(node));
    out.extend_from_slice(&number.to_be_bytes());
    out.extend_from_slice(&serial.to_be_bytes());
    out.push(creation);
    out
}

/// `[NEW_PID_EXT][node][number:4][serial:4][creation:4]` (`external.c:3165-3170`) —
/// the modern form, with a full 32-bit creation. Same `number` then `serial` order.
fn new_pid_ext(node: &str, number: u32, serial: u32, creation: u32) -> Vec<u8> {
    let mut out = vec![NEW_PID_EXT];
    out.extend_from_slice(&atom_bytes(node));
    out.extend_from_slice(&number.to_be_bytes());
    out.extend_from_slice(&serial.to_be_bytes());
    out.extend_from_slice(&creation.to_be_bytes());
    out
}

/// `[PORT_EXT][node][id:4][creation:1]` (`external.c:4685-4716`).
fn port_ext(node: &str, id: u32, creation: u8) -> Vec<u8> {
    let mut out = vec![PORT_EXT];
    out.extend_from_slice(&atom_bytes(node));
    out.extend_from_slice(&id.to_be_bytes());
    out.push(creation);
    out
}

/// `[NEW_PORT_EXT][node][id:4][creation:4]` (`external.c:4712-4716`).
fn new_port_ext(node: &str, id: u32, creation: u32) -> Vec<u8> {
    let mut out = vec![NEW_PORT_EXT];
    out.extend_from_slice(&atom_bytes(node));
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&creation.to_be_bytes());
    out
}

/// `[V4_PORT_EXT][node][id:8][creation:4]` (`external.c:4698-4716`) — the form the
/// C encoder always emits for a port.
fn v4_port_ext(node: &str, id: u64, creation: u32) -> Vec<u8> {
    let mut out = vec![V4_PORT_EXT];
    out.extend_from_slice(&atom_bytes(node));
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&creation.to_be_bytes());
    out
}

/// `[NEWER_REFERENCE_EXT][len:2][node][creation:4][ids…]` (`external.c:4786-4796`,
/// `external.h`).
///
/// Note the field order: the **creation precedes the numbers** in this tag, and
/// `len` is a 2-byte big-endian count.
fn newer_ref_ext(node: &str, creation: u32, ids: &[u32]) -> Vec<u8> {
    let mut out = vec![NEWER_REFERENCE_EXT];
    out.extend_from_slice(&(ids.len() as u16).to_be_bytes());
    out.extend_from_slice(&atom_bytes(node));
    out.extend_from_slice(&creation.to_be_bytes());
    for id in ids {
        out.extend_from_slice(&id.to_be_bytes());
    }
    out
}

/// `[SMALL_TUPLE_EXT][arity][SMALL_INTEGER_EXT][opcode][args…][message?]` — the
/// body-form control message ([`decode_control_with_identity`]'s input).
fn control_body(dop: Dop, args: &[Vec<u8>], message: Option<Vec<u8>>) -> Vec<u8> {
    let mut out = vec![SMALL_TUPLE_EXT, (args.len() + 1) as u8, SMALL_INTEGER_EXT, dop.as_u8()];
    for arg in args {
        out.extend_from_slice(arg);
    }
    if let Some(message) = message {
        out.extend_from_slice(&message);
    }
    out
}

/// A `{reply, Payload}` message tuple.
fn reply_tuple(payload: Vec<u8>) -> Vec<u8> {
    let mut out = vec![SMALL_TUPLE_EXT, 2];
    out.extend_from_slice(&atom_bytes("reply"));
    out.extend_from_slice(&payload);
    out
}

// ---------------------------------------------------------------------------
// The decode fixture.
// ---------------------------------------------------------------------------

/// A decode fixture: the production-shaped `beam_alloc` heap plus the node's
/// `AtomTable` (the table the identity's atom must come from).
struct Fixture {
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture { heap: Heap::new(4096).expect("4096-word test heap"), atoms: AtomTable::new() }
    }

    /// This node's identity, interned into the fixture's own table.
    fn identity(&mut self) -> LocalIdentity {
        LocalIdentity::intern(&mut self.atoms, LOCAL_NODE.as_bytes(), LOCAL_CREATION)
            .expect("intern the local node identity")
    }

    /// Decodes with `identity` installed (or without one when `None`).
    fn decode(&mut self, body: &[u8], identity: Option<LocalIdentity>) -> ControlMessage {
        decode_control_with_identity(body, &mut self.heap, &mut self.atoms, None, identity)
            .unwrap_or_else(|err| panic!("decode failed: {err:?}"))
    }

    /// Decodes **without** an identity — the identity-less (pre-seam) behaviour.
    fn decode_plain(&mut self, body: &[u8]) -> ControlMessage {
        decode_control(body, &mut self.heap, &mut self.atoms, None)
            .unwrap_or_else(|err| panic!("identity-less decode failed: {err:?}"))
    }
}

/// `{DOP_SEND, From(peer pid), To(this-node pid)} -- Payload` — the envelope most
/// payload tests use.
fn send_with_payload(payload: Vec<u8>) -> Vec<u8> {
    control_body(
        Dop::Send,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            new_pid_ext(LOCAL_NODE, 9, 1, LOCAL_CREATION),
        ],
        Some(payload),
    )
}

/// `is_external_pid(term)` (the safe `beam_codec` reader).
fn is_external_pid(term: Eterm) -> bool {
    external_pid_parts(term).is_ok()
}

/// `is_external_port(term)`.
fn is_external_port(term: Eterm) -> bool {
    external_port_parts(term).is_ok()
}

/// `is_external_ref(term)`.
fn is_external_ref(term: Eterm) -> bool {
    external_ref_parts(term).is_ok()
}

// ---------------------------------------------------------------------------
// (1) Pids — both wire tags, `PID_EXT` and `NEW_PID_EXT`.
// ---------------------------------------------------------------------------

#[test]
fn a_this_node_pid_decodes_internal_on_both_wire_tags() {
    // `{DOP_LINK, From, To}`: `From` is the peer's pid (external, never demoted),
    // `To` is a pid of **this** node — it must decode to the internal pid immediate
    // so `To =:= self()` can hold for the same number/serial.
    for to in [
        pid_ext(LOCAL_NODE, 42, 5, LOCAL_CREATION as u8),
        new_pid_ext(LOCAL_NODE, 42, 5, LOCAL_CREATION),
    ] {
        let body = control_body(
            Dop::Link,
            &[new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION), to.clone()],
            None,
        );

        let mut fixture = Fixture::new();
        let identity = fixture.identity();
        let message = fixture.decode(&body, Some(identity));

        assert_eq!(message.dop, Dop::Link);
        let from = message.terms[0];
        let to = message.terms[1];

        // The peer's pid stays external (it is not this node).
        assert!(is_external_pid(from), "a peer pid stays external");
        assert!(!is_internal_pid(from));

        // This node's pid is the internal immediate, built from (serial, number) —
        // the wire wrote `number` first, then `serial`.
        assert!(is_internal_pid(to), "a this-node pid decodes internal");
        assert_eq!(to, make_internal_pid(make_pid_data(5, 42)));
        assert_eq!(pid_number(to), 42);
        assert_eq!(pid_serial(to), 5);
    }
}

#[test]
fn the_wire_order_is_number_then_serial() {
    // A pid whose two fields differ on both tags: an implementation that swapped
    // them would still be "an internal pid", so the fields are asserted one by one
    // (the objective's wire-order note, made executable).
    let body = control_body(
        Dop::Link,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            pid_ext(LOCAL_NODE, 7, 3, LOCAL_CREATION as u8),
        ],
        None,
    );
    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let to = fixture.decode(&body, Some(identity)).terms[1];
    assert_eq!((pid_number(to), pid_serial(to)), (7, 3));
}

#[test]
fn a_foreign_node_or_a_foreign_creation_stays_external() {
    // The C compares `sysname` **and** `creation` (`dec_is_this_node`,
    // `external.c:3123-3129`): only both matching is local.
    for to in [
        // A different node.
        new_pid_ext(OTHER_NODE, 42, 5, LOCAL_CREATION),
        // This node's name, another incarnation.
        new_pid_ext(LOCAL_NODE, 42, 5, LOCAL_CREATION + 1),
        // A different node *and* another creation.
        pid_ext(OTHER_NODE, 42, 5, 0),
    ] {
        let body =
            control_body(Dop::Link, &[new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION), to], None);
        let mut fixture = Fixture::new();
        let identity = fixture.identity();
        let message = fixture.decode(&body, Some(identity));
        let to = message.terms[1];
        assert!(is_external_pid(to), "a non-local pid keeps its external form");
        assert!(!is_internal_pid(to));
        // The faithful external form still carries the wire's number/serial.
        let (_, number, serial, _) = external_pid_parts(to).expect("external pid");
        assert_eq!((number, serial), (42, 5));
    }
}

#[test]
fn the_identity_engages_the_local_pid_bounds() {
    // D6: a *local* pid whose number exceeds `ERTS_MAX_INTERNAL_PID_NUMBER`
    // (`2^28 - 1`) is `badarg` (`dec_pid`, `external.c:3175-3179`) — the same image
    // on a *foreign* node is a perfectly good external pid.
    let number = 1u32 << 28;
    let body = control_body(
        Dop::Link,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            new_pid_ext(LOCAL_NODE, number, 0, LOCAL_CREATION),
        ],
        None,
    );

    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let err = decode_control_with_identity(&body, &mut fixture.heap, &mut fixture.atoms, None, Some(identity))
        .expect_err("a local pid number past the internal bound is badarg");
    assert!(matches!(err, BeamError::BadArg), "expected badarg, got {err:?}");

    // Without an identity the very same bytes decode: it is a foreign pid, and a
    // foreign pid has no internal bound.
    let mut plain = Fixture::new();
    let message = plain.decode_plain(&body);
    assert!(is_external_pid(message.terms[1]));
}

// ---------------------------------------------------------------------------
// (2) The trailing message payload.
// ---------------------------------------------------------------------------

#[test]
fn a_send_payload_pid_decodes_internal() {
    // `{DOP_SEND, From, To} -- Message` — one decoder covers the control arguments
    // *and* the trailing message, so a payload pid is demoted too.
    let local_pid = new_pid_ext(LOCAL_NODE, 9, 1, LOCAL_CREATION);
    let body = send_with_payload(reply_tuple(local_pid.clone()));

    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let message = fixture.decode(&body, Some(identity));
    assert_eq!(message.terms.len(), 3, "two control args plus the trailing message");

    let payload = message.terms[2];
    let payload_pid = beam_codec::term::tuple_element(payload, 1).expect("payload element 2");
    assert!(is_internal_pid(payload_pid), "the payload pid decodes internal");
    assert_eq!(payload_pid, make_internal_pid(make_pid_data(1, 9)));

    // ... while the peer's pid in the control tuple stays external.
    assert!(is_external_pid(message.terms[0]));
}

#[test]
fn every_send_family_payload_pid_decodes_internal() {
    // The same rule across the send family the C encodes a trailing message for
    // (`dist.c:2468`, `:2486`, `:2584`, `:3194-3199`).
    let local_pid = reply_tuple(new_pid_ext(LOCAL_NODE, 9, 1, LOCAL_CREATION));
    let peer_pid = new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION);

    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "DOP_SEND",
            control_body(Dop::Send, &[peer_pid.clone(), local_pid.clone()], Some(local_pid.clone())),
        ),
        (
            "DOP_SEND_SENDER",
            control_body(
                Dop::SendSender,
                &[peer_pid.clone(), local_pid.clone()],
                Some(local_pid.clone()),
            ),
        ),
        (
            "DOP_REG_SEND",
            control_body(
                Dop::RegSend,
                &[peer_pid.clone(), atom_bytes("cookie"), atom_bytes("name")],
                Some(local_pid.clone()),
            ),
        ),
        (
            "DOP_ALIAS_SEND",
            control_body(
                Dop::AliasSend,
                &[
                    peer_pid,
                    newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, &[3, 0, 0]),
                ],
                Some(local_pid.clone()),
            ),
        ),
    ];

    for (what, body) in cases {
        let mut fixture = Fixture::new();
        let identity = fixture.identity();
        let message = fixture.decode(&body, Some(identity));
        let payload = *message.terms.last().expect("a trailing message");
        let payload_pid = beam_codec::term::tuple_element(payload, 1).expect("payload element 2");
        assert!(is_internal_pid(payload_pid), "{what}: the payload pid decodes internal");
        assert_eq!(
            payload_pid,
            make_internal_pid(make_pid_data(1, 9)),
            "{what}: same number/serial as the wire"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) References.
// ---------------------------------------------------------------------------

#[test]
fn a_this_node_reference_decodes_internal() {
    // `NEWER_REFERENCE_EXT` of this node → the internal reference, so an inbound
    // alias (`DOP_ALIAS_SEND`'s `To`) is a reference the runtime can resolve.
    let ids = [3u32, 0, 0];
    let body = control_body(
        Dop::AliasSend,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, &ids),
        ],
        Some(reply_tuple(vec![SMALL_INTEGER_EXT, 7])),
    );

    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let message = fixture.decode(&body, Some(identity));
    let to = message.terms[1];
    assert_eq!(internal_ref_id(to).expect("an internal reference"), 3);
}

#[test]
fn a_foreign_reference_or_an_unrepresentable_id_stays_external() {
    // Another node's reference: external.
    let other = control_body(
        Dop::AliasSend,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            newer_ref_ext(OTHER_NODE, OTHER_CREATION, &[3, 0, 0]),
        ],
        Some(reply_tuple(vec![SMALL_INTEGER_EXT, 7])),
    );
    // This node, another incarnation: external (the creation check).
    let old_incarnation = control_body(
        Dop::AliasSend,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            newer_ref_ext(LOCAL_NODE, LOCAL_CREATION + 1, &[3, 0, 0]),
        ],
        Some(reply_tuple(vec![SMALL_INTEGER_EXT, 7])),
    );
    // This node, but an id that does not fit the internal form's 60-bit slot
    // (`num[2] = 2^28` ⇒ id `2^60`): the faithful **external** form is kept — a
    // documented fallback, never a truncated id and never an error
    // (`beam_codec::ref_wire::representable_ref_id`).
    let too_large = control_body(
        Dop::AliasSend,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, &[3, 0, 1 << 28]),
        ],
        Some(reply_tuple(vec![SMALL_INTEGER_EXT, 7])),
    );

    for (what, body) in
        [("other node", other), ("old incarnation", old_incarnation), ("unrepresentable", too_large)]
    {
        let mut fixture = Fixture::new();
        let identity = fixture.identity();
        let message = fixture.decode(&body, Some(identity));
        let to = message.terms[1];
        assert!(is_external_ref(to), "{what}: the reference keeps its external form");
        assert!(internal_ref_id(to).is_err(), "{what}: no internal reference is fabricated");
    }
}

// ---------------------------------------------------------------------------
// (4) Ports.
// ---------------------------------------------------------------------------

#[test]
fn a_this_node_port_decodes_internal() {
    // A port inside a message payload — the demotion is term-graph-wide, and the
    // port has no control-tuple slot of its own in v6.
    for port in [
        port_ext(LOCAL_NODE, 77, LOCAL_CREATION as u8),
        new_port_ext(LOCAL_NODE, 77, LOCAL_CREATION),
        v4_port_ext(LOCAL_NODE, 77, LOCAL_CREATION),
    ] {
        let body = send_with_payload(port);
        let mut fixture = Fixture::new();
        let identity = fixture.identity();
        let message = fixture.decode(&body, Some(identity));
        let payload = message.terms[2];
        assert!(is_internal_port(payload), "a this-node port decodes internal");
        assert_eq!(internal_port_data(payload), 77);
    }
}

#[test]
fn a_foreign_port_stays_external() {
    for port in [
        port_ext(OTHER_NODE, 77, OTHER_CREATION as u8),
        new_port_ext(OTHER_NODE, 77, OTHER_CREATION),
        v4_port_ext(OTHER_NODE, 77, OTHER_CREATION),
        // This node, another incarnation.
        v4_port_ext(LOCAL_NODE, 77, LOCAL_CREATION + 1),
    ] {
        let body = send_with_payload(port);
        let mut fixture = Fixture::new();
        let identity = fixture.identity();
        let message = fixture.decode(&body, Some(identity));
        let payload = message.terms[2];
        assert!(is_external_port(payload), "a non-local port keeps its external form");
        assert!(!is_internal_port(payload));
        assert_eq!(external_port_parts(payload).expect("external port").1, 77);
    }
}

// ---------------------------------------------------------------------------
// (5) Classification of a demoted alias send.
// ---------------------------------------------------------------------------

#[test]
fn alias_send_carries_the_demoted_internal_reference() {
    // `{DOP_ALIAS_SEND, From, To} -- Message` (`dist.c:2563-2587`): `To` is the
    // alias reference, which lives on **this** node — on the wire it is an external
    // reference naming this node, and the runtime can only resolve an *internal*
    // one.
    let ids = [11u32, 0, 0];
    let body = control_body(
        Dop::AliasSend,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, &ids),
        ],
        Some(reply_tuple(vec![SMALL_INTEGER_EXT, 7])),
    );

    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let message = fixture.decode(&body, Some(identity));

    match Delivery::classify(&message).expect("classify") {
        Delivery::AliasSend { to, .. } => {
            assert_eq!(internal_ref_id(to).expect("internal alias reference"), 11);
        }
        other => panic!("expected an alias send, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (6) Additivity: with no identity, every identifier keeps its external form.
// ---------------------------------------------------------------------------

#[test]
fn without_an_identity_every_identifier_stays_external() {
    let local_pid = new_pid_ext(LOCAL_NODE, 9, 1, LOCAL_CREATION);
    let local_ref = newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, &[3, 0, 0]);
    let local_port = v4_port_ext(LOCAL_NODE, 77, LOCAL_CREATION);

    // `{DOP_LINK, peer, this-node pid}` — both decode sites, no identity.
    let link = control_body(
        Dop::Link,
        &[new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION), local_pid.clone()],
        None,
    );
    // `{DOP_ALIAS_SEND, peer, this-node ref} -- {reply, this-node pid | port}`
    let alias = control_body(
        Dop::AliasSend,
        &[new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION), local_ref.clone()],
        Some(reply_tuple(local_pid.clone())),
    );
    let port_message = send_with_payload(local_port.clone());

    // `decode_control` (the identity-less entry point) and
    // `decode_control_with_identity(…, None)` must agree, and both keep the
    // faithful external forms. (Two decodes of the same bytes are two distinct
    // heap allocations, so the *fields* are compared, not the term words.)
    let mut fixture = Fixture::new();
    let plain = fixture.decode_plain(&link);
    assert!(is_external_pid(plain.terms[1]));
    assert!(!is_internal_pid(plain.terms[1]));
    assert_eq!(external_pid_parts(plain.terms[1]).expect("external pid").1, 9);
    let again = fixture.decode(&link, None);
    assert_eq!(
        external_pid_parts(plain.terms[1]).expect("external pid"),
        external_pid_parts(again.terms[1]).expect("external pid")
    );

    let plain = fixture.decode_plain(&alias);
    assert!(is_external_ref(plain.terms[1]));
    assert!(internal_ref_id(plain.terms[1]).is_err());
    let payload_pid = beam_codec::term::tuple_element(plain.terms[2], 1).expect("payload pid");
    assert!(is_external_pid(payload_pid));

    let plain = fixture.decode_plain(&port_message);
    assert!(is_external_port(plain.terms[2]));
}

// ---------------------------------------------------------------------------
// (7) The `LocalIdentity` value itself.
// ---------------------------------------------------------------------------

#[test]
fn intern_rejects_a_non_utf8_sysname() {
    let mut atoms = AtomTable::new();
    let err = LocalIdentity::intern(&mut atoms, &[0xff, 0xfe], LOCAL_CREATION)
        .expect_err("a non-UTF-8 sysname is badarg");
    assert!(matches!(err, BeamError::BadArg), "expected badarg, got {err:?}");
}

#[test]
fn new_and_intern_agree() {
    let mut atoms = AtomTable::new();
    let interned =
        LocalIdentity::intern(&mut atoms, LOCAL_NODE.as_bytes(), LOCAL_CREATION).expect("intern");
    let direct = LocalIdentity::new(atoms.intern(LOCAL_NODE), LOCAL_CREATION);
    assert_eq!(interned, direct);
    assert_eq!(interned.creation, LOCAL_CREATION);
}

// ---------------------------------------------------------------------------
// (8) The per-connection leg, end to end over a real loopback socket.
// ---------------------------------------------------------------------------

/// The two live endpoints of one loopback link, both owned by the test thread.
struct Pair {
    /// The accepting node — the identity-under-test side.
    alpha: Connection<TcpTransport>,
    /// The connecting node.
    beta: Connection<TcpTransport>,
}

/// Binds a loopback listener, completes the `'N'` handshake on the acceptor thread
/// and returns both established endpoints (`tests/validated_delivery.rs`'s pattern).
fn connect_pair() -> Pair {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let acceptor = thread::spawn(move || -> beam_common::Result<Connection<TcpTransport>> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server(transport, LOCAL_NODE.as_bytes(), COOKIE, LOCAL_CREATION)
    });

    let mut transport = TcpTransport::connect(&addr.to_string()).expect("connect to the peer");
    transport.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    let beta = Connection::client_with_creation(
        transport,
        OTHER_NODE.as_bytes(),
        COOKIE,
        OTHER_CREATION,
    )
    .expect("connecting handshake");
    let alpha = acceptor.join().expect("acceptor thread").expect("accepting handshake");
    Pair { alpha, beta }
}

#[test]
fn the_connection_recv_demotes_a_this_node_pid_and_clearing_restores_it() {
    let mut pair = connect_pair();
    assert_eq!(pair.alpha.local_identity(), None, "a fresh link has no identity");

    // The peer sends a `DOP_LINK` whose `To` is a pid it believes is ours — the
    // exact shape a real node produces for `erlang:link(Pid)` against us.
    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = beam_codec::term::make_external_pid(
        &mut beta_heap,
        beam_term::prelude::make_atom(beta_atoms.intern(OTHER_NODE)),
        1,
        0,
        OTHER_CREATION,
    )
    .expect("peer pid");
    let to_for = |heap: &mut Heap, atoms: &mut AtomTable, creation: u32| {
        beam_codec::term::make_external_pid(
            heap,
            beam_term::prelude::make_atom(atoms.intern(LOCAL_NODE)),
            9,
            1,
            creation,
        )
        .expect("this-node pid")
    };
    let to = to_for(&mut beta_heap, &mut beta_atoms, LOCAL_CREATION);
    pair.beta.send_link(from, to, &beta_atoms).expect("send DOP_LINK");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let identity =
        LocalIdentity::intern(&mut alpha_atoms, LOCAL_NODE.as_bytes(), LOCAL_CREATION).expect("id");
    pair.alpha.set_local_identity(identity);
    assert_eq!(pair.alpha.local_identity(), Some(identity));

    let message = pair.alpha.recv(&mut alpha_heap, &mut alpha_atoms).expect("recv");
    assert_eq!(message.dop, Dop::Link);
    assert!(is_internal_pid(message.terms[1]), "the this-node pid decodes internal");
    assert_eq!(message.terms[1], make_internal_pid(make_pid_data(1, 9)));

    // Clearing the identity restores the faithful external form for the next
    // message — and `=:=` no longer holds against the internal pid.
    pair.alpha.clear_local_identity();
    assert_eq!(pair.alpha.local_identity(), None);
    let to = to_for(&mut beta_heap, &mut beta_atoms, LOCAL_CREATION);
    pair.beta.send_link(from, to, &beta_atoms).expect("send DOP_LINK again");
    let message = pair.alpha.recv(&mut alpha_heap, &mut alpha_atoms).expect("recv");
    assert!(is_external_pid(message.terms[1]));
    assert_ne!(message.terms[1], make_internal_pid(make_pid_data(1, 9)));
}

// ---------------------------------------------------------------------------
// (9) The non-blocking driver leg.
// ---------------------------------------------------------------------------

/// Drives two [`ConnectionDriver`]s against each other until both are established
/// (the `tests/driver.rs` byte shuttle).
fn establish_drivers() -> (ConnectionDriver, ConnectionDriver) {
    let mut beta = ConnectionDriver::client(OTHER_NODE.as_bytes(), COOKIE, OTHER_CREATION);
    let mut alpha = ConnectionDriver::server(LOCAL_NODE.as_bytes(), COOKIE, LOCAL_CREATION);

    for _ in 0..100_000 {
        let beta_out = beta.take_output();
        let alpha_out = alpha.take_output();
        if !beta_out.is_empty() {
            alpha.feed(&beta_out);
        }
        if !alpha_out.is_empty() {
            beta.feed(&alpha_out);
        }
        beta.step().expect("client step");
        alpha.step().expect("server step");

        if alpha.state() == DriverState::Established && beta.state() == DriverState::Established {
            return (alpha, beta);
        }
        if alpha.state() == DriverState::Failed || beta.state() == DriverState::Failed {
            panic!("driver failed: alpha={:?}, beta={:?}", alpha.state(), beta.state());
        }
    }
    panic!("the driver handshake did not complete");
}

#[test]
fn the_driver_next_message_demotes_a_this_node_pid() {
    let (mut alpha, mut beta) = establish_drivers();
    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = beam_codec::term::make_external_pid(
        &mut beta_heap,
        beam_term::prelude::make_atom(beta_atoms.intern(OTHER_NODE)),
        1,
        0,
        OTHER_CREATION,
    )
    .expect("peer pid");
    let to = beam_codec::term::make_external_pid(
        &mut beta_heap,
        beam_term::prelude::make_atom(beta_atoms.intern(LOCAL_NODE)),
        9,
        1,
        LOCAL_CREATION,
    )
    .expect("this-node pid");
    beta.send_control(Dop::Link, &[from, to], &beta_atoms).expect("send DOP_LINK");
    alpha.feed(&beta.take_output());
    alpha.step().expect("server step");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let identity =
        LocalIdentity::intern(&mut alpha_atoms, LOCAL_NODE.as_bytes(), LOCAL_CREATION).expect("id");
    alpha.set_local_identity(identity);
    let message = alpha
        .next_message(&mut alpha_heap, &mut alpha_atoms)
        .expect("next_message")
        .expect("one queued message");
    assert_eq!(message.dop, Dop::Link);
    assert!(is_internal_pid(message.terms[1]), "the driver demotes too");
    assert_eq!(message.terms[1], make_internal_pid(make_pid_data(1, 9)));

    // `clear_local_identity` restores the external form on the driver as well.
    alpha.clear_local_identity();
    assert_eq!(alpha.local_identity(), None);
    let to = beam_codec::term::make_external_pid(
        &mut beta_heap,
        beam_term::prelude::make_atom(beta_atoms.intern(LOCAL_NODE)),
        9,
        1,
        LOCAL_CREATION,
    )
    .expect("this-node pid");
    beta.send_control(Dop::Link, &[from, to], &beta_atoms).expect("send DOP_LINK again");
    alpha.feed(&beta.take_output());
    alpha.step().expect("server step");
    let message = alpha
        .next_message(&mut alpha_heap, &mut alpha_atoms)
        .expect("next_message")
        .expect("one queued message");
    assert!(is_external_pid(message.terms[1]));
}

// ---------------------------------------------------------------------------
// (10) The **validated** (per-link) view of a demoted reference.
//
// This is the acceptance criterion of the decode-side identity seam: an inbound
// reference naming this node must (a) decode to the **internal** reference and
// (b) then pass `Delivery::classify` *and* `Delivery::classify_validated`, whose
// reference guards are the C's `is_not_ref` — which spans internal and external
// references (`dist.c:2584` for `DOP_ALIAS_SEND`, `dist.c:2723` for
// `DOP_[PAYLOAD_]MONITOR_P_EXIT`). The delivered reference is the *demoted* term
// itself, never a substituted one.
// ---------------------------------------------------------------------------

/// The per-link identity for the fixture's own atom table: the peer is
/// [`OTHER_NODE`], this node is [`LOCAL_NODE`] (the table the decode used).
fn link_identity(fixture: &mut Fixture) -> LinkIdentity {
    LinkIdentity::intern(&mut fixture.atoms, OTHER_NODE.as_bytes(), LOCAL_NODE.as_bytes())
        .expect("intern the link identity")
}

/// `{DOP_ALIAS_SEND, peer, <this-node reference>} -- {reply,7}` — an inbound alias
/// whose alias reference is this node's own. `ids` is the wire reference's number
/// triple.
fn alias_send_body(ids: &[u32]) -> Vec<u8> {
    control_body(
        Dop::AliasSend,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, ids),
        ],
        Some(reply_tuple(vec![SMALL_INTEGER_EXT, 7])),
    )
}

/// `{DOP_MONITOR_P_EXIT, peer, <this-node pid>, <this-node reference>, 3}` — a
/// monitor down signal whose reference is this node's own.
fn monitor_p_exit_body(ids: &[u32]) -> Vec<u8> {
    control_body(
        Dop::MonitorPExit,
        &[
            new_pid_ext(OTHER_NODE, 1, 0, OTHER_CREATION),
            new_pid_ext(LOCAL_NODE, 9, 1, LOCAL_CREATION),
            newer_ref_ext(LOCAL_NODE, LOCAL_CREATION, ids),
            vec![SMALL_INTEGER_EXT, 3],
        ],
        None,
    )
}

#[test]
fn an_inbound_alias_send_to_this_node_validates_as_an_internal_reference() {
    let ids = [11u32, 0, 0];
    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let message = fixture.decode(&alias_send_body(&ids), Some(identity));
    let link = link_identity(&mut fixture);

    let to = message.terms[1];
    assert_eq!(internal_ref_id(to).expect("the demoted internal reference"), 11);

    // The semantic view carries the internal reference...
    match Delivery::classify(&message).expect("classify") {
        Delivery::AliasSend { to: classified, .. } => assert_eq!(classified, to),
        other => panic!("expected an alias send, got {other:?}"),
    }
    // ...and the validated view accepts it instead of rejecting a wire form the C
    // accepts (`is_not_ref(to)`, `dist.c:2584`).
    match Delivery::classify_validated(&message, &link)
        .expect("an internal alias reference passes the validated view (`dist.c:2584`)")
    {
        Delivery::AliasSend { to: validated, .. } => {
            assert_eq!(internal_ref_id(validated).expect("still the internal reference"), 11);
            assert_eq!(validated, to, "the demoted term itself is delivered");
        }
        other => panic!("expected an alias send, got {other:?}"),
    }
}

#[test]
fn an_inbound_monitor_p_exit_of_this_nodes_reference_validates_as_internal() {
    let ids = [11u32, 0, 0];
    let mut fixture = Fixture::new();
    let identity = fixture.identity();
    let message = fixture.decode(&monitor_p_exit_body(&ids), Some(identity));
    let link = link_identity(&mut fixture);

    let reference = message.terms[2];
    assert_eq!(internal_ref_id(reference).expect("the demoted internal reference"), 11);
    assert!(is_internal_pid(message.terms[1]), "the local watcher is an internal pid");

    // `is_not_ref(ref)` (`dist.c:2723`) admits the internal form, so the monitor
    // down signal this node's own reference belongs to is not `invalid_message`.
    match Delivery::classify_validated(&message, &link)
        .expect("an internal monitor reference passes the validated view (`dist.c:2723`)")
    {
        Delivery::MonitorExit { reference: validated, .. } => {
            assert_eq!(internal_ref_id(validated).expect("still the internal reference"), 11);
            assert_eq!(validated, reference, "the demoted term itself is delivered");
        }
        other => panic!("expected a monitor exit, got {other:?}"),
    }
}

#[test]
fn without_an_identity_the_widened_reference_arms_accept_the_external_form() {
    // Additivity: with no identity installed the same bytes keep the faithful
    // **external** reference (no internal reference is fabricated) and the validated
    // view accepts it through the external arm — the widening changes nothing here.
    let ids = [11u32, 0, 0];
    let mut fixture = Fixture::new();
    let link = link_identity(&mut fixture);

    let message = fixture.decode_plain(&alias_send_body(&ids));
    let to = message.terms[1];
    assert!(is_external_ref(to), "no identity ⇒ the faithful external form");
    assert!(internal_ref_id(to).is_err(), "no internal reference is fabricated");
    match Delivery::classify_validated(&message, &link).expect("the external arm is unchanged") {
        Delivery::AliasSend { to: validated, .. } => assert_eq!(validated, to),
        other => panic!("expected an alias send, got {other:?}"),
    }

    let message = fixture.decode_plain(&monitor_p_exit_body(&ids));
    let reference = message.terms[2];
    assert!(is_external_ref(reference), "no identity ⇒ the faithful external form");
    assert!(internal_ref_id(reference).is_err());
    match Delivery::classify_validated(&message, &link).expect("the external arm is unchanged") {
        Delivery::MonitorExit { reference: validated, .. } => assert_eq!(validated, reference),
        other => panic!("expected a monitor exit, got {other:?}"),
    }
}
