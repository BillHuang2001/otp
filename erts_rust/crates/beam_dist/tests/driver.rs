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

//! Hermetic tests for the driven (poll/step) distribution connection state
//! machine ([`beam_dist::driver`]).
//!
//! No sockets are involved: a **client** and a **server** [`ConnectionDriver`]
//! are wired to each other and their [`ConnectionDriver::take_output`] bytes are
//! shuttled by hand — either wholesale or one byte per `feed` (to prove the
//! parsing is genuinely incremental). Every expectation is an observable outcome
//! (state, negotiated flags, decoded control message), never an internal detail.
//!
//! The reported requirements covered here:
//! * handshake completes and reports negotiated flags / peer names / creations;
//! * the same handshake completes **byte-at-a-time**;
//! * a full `DOP_*` round trip both directions with atoms, exercising the atom
//!   cache (first message `New` refs, second `Cached` refs), and that an atom-free
//!   message carries the exact empty header `131, 68, 0`;
//! * a message larger than [`DIST_FRAGMENT_SIZE`] round-trips both ways (`'E'`/`'F'`);
//! * a tick is answered with a tick in `take_output`;
//! * malformed input (garbage / truncated / empty frame) yields a typed `Err`, no
//!   panic, and a terminal [`DriverState::Failed`];
//! * the handshake **error** paths mirror the blocking API: a peer `Status`
//!   refusal is classified through the shared mapping
//!   ([`beam_dist::handshake::status_error`]), and a refusing **acceptor** buffers
//!   the 2-byte-framed `"not_allowed"` status into `take_output` before failing.

use beam_alloc::Heap;
use beam_codec::AtomEncoding;
use beam_common::BeamError;
use beam_dist::dist_header::{AtomCacheRef, DistHeader, DIST_FRAG_HEADER, DIST_HEADER};
use beam_dist::driver::{ConnectionDriver, DriverState, StepOutcome};
use beam_dist::flags::{DistFlags, Dop, DFLAG_FRAGMENTS, DFLAG_PUBLISHED};
use beam_dist::fragment::DIST_FRAGMENT_SIZE;
use beam_dist::handshake::{publish_flag, status_error, PeerVisibility};
use beam_dist::{
    check_mandatory, preferred_flags, write_packet, MemoryTransport, Name, PacketHeaderSize, Status,
    StatusKind,
};
use beam_term::prelude::{atom_val, make_atom, make_small, Eterm, NIL};
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

/// How the shuttle loop moves bytes between the two drivers.
#[derive(Clone, Copy)]
enum Mode {
    /// Feed each side's whole output buffer in one `feed`.
    Whole,
    /// Feed each side's output **one byte at a time**, stepping after every byte.
    ByteAtATime,
}

/// Drives a client and a server driver against each other until both are
/// established, each advertising [`preferred_flags`].
fn establish(mode: Mode) -> (ConnectionDriver, ConnectionDriver) {
    establish_with(mode, preferred_flags(), preferred_flags())
}

/// [`establish`] with an explicit advertised capability set for each side (the
/// opt-in `*_with_flags` constructors).
fn establish_with(
    mode: Mode,
    client_flags: DistFlags,
    server_flags: DistFlags,
) -> (ConnectionDriver, ConnectionDriver) {
    let mut client =
        ConnectionDriver::client_with_flags(CLIENT_NAME, COOKIE, CLIENT_CREATION, client_flags);
    let mut server =
        ConnectionDriver::server_with_flags(SERVER_NAME, COOKIE, SERVER_CREATION, server_flags);

    for _ in 0..100_000 {
        let client_out = client.take_output();
        let server_out = server.take_output();
        match mode {
            Mode::Whole => {
                if !client_out.is_empty() {
                    server.feed(&client_out);
                }
                if !server_out.is_empty() {
                    client.feed(&server_out);
                }
            }
            Mode::ByteAtATime => {
                for byte in client_out {
                    server.feed(&[byte]);
                    server.step().expect("server step");
                }
                for byte in server_out {
                    client.feed(&[byte]);
                    client.step().expect("client step");
                }
            }
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
    panic!(
        "handshake did not complete: client={:?}, server={:?}",
        client.state(),
        server.state()
    );
}

/// The body-form external term image of `term` (no `VERSION_MAGIC`).
fn body_image(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    beam_codec::encode_term_body(term, atoms).expect("encode term body")
}

/// Builds a proper list of `len` small ints `start .. start + len` on `heap`.
fn big_list(heap: &mut Heap, len: i64, start: i64) -> Eterm {
    let mut list = NIL;
    for i in (0..len).rev() {
        list = heap.cons(make_small(start + i), list).expect("cons");
    }
    list
}

/// Decodes the distribution header of a single 4-byte-framed established packet
/// `bytes` (the client's `take_output` for one small message).
fn decode_single_frame_header(bytes: &[u8]) -> DistHeader {
    assert!(bytes.len() >= 6, "a frame needs at least a length prefix + header start");
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(bytes.len(), 4 + len, "exactly one frame was expected");
    let body = &bytes[4..];
    assert_eq!(body[0], beam_codec::VERSION_MAGIC);
    assert_eq!(body[1], DIST_HEADER);
    let mut atoms = AtomTable::new();
    let (header, _offset) = DistHeader::decode_tail(body, 2, &mut atoms, AtomEncoding::Utf8)
        .expect("decode the distribution header");
    header
}

// ---------------------------------------------------------------------------
// (a) The handshake establishes an agreed link and reports the peer.
// ---------------------------------------------------------------------------

#[test]
fn handshake_establishes_and_reports_the_peer() {
    let (client, server) = establish(Mode::Whole);

    let agreed = check_mandatory(preferred_flags()).expect("the preferred set is conforming");
    assert_eq!(client.state(), DriverState::Established);
    assert_eq!(server.state(), DriverState::Established);

    assert_eq!(client.client_flags(), preferred_flags());
    assert_eq!(client.server_flags(), preferred_flags());
    assert_eq!(server.client_flags(), preferred_flags());
    assert_eq!(server.server_flags(), preferred_flags());
    assert_eq!(client.negotiated(), agreed);
    assert_eq!(server.negotiated(), agreed);

    // Each end learns the other's name and creation.
    assert_eq!(client.peer_name(), Some(SERVER_NAME));
    assert_eq!(server.peer_name(), Some(CLIENT_NAME));
    assert_eq!(client.peer_creation(), SERVER_CREATION);
    assert_eq!(server.peer_creation(), CLIENT_CREATION, "the client advertised its creation");

    // The full handshake outcome is exposed and mirrors the blocking side.
    let handshake = client.handshake().expect("the handshake outcome");
    assert_eq!(handshake.negotiated, agreed);
    assert_eq!(handshake.peer_name, SERVER_NAME);

    // The negotiated set governs outbound encoding.
    assert!(client.negotiated().contains(DistFlags::from_bits(DFLAG_FRAGMENTS)));

    // GOLDEN-VECTOR: neither default constructor advertises `DFLAG_PUBLISHED`, so
    // each side classifies the other as hidden (`dist_util.erl:publish_type/1`).
    assert_eq!(client.peer_visibility(), Some(PeerVisibility::Hidden));
    assert_eq!(server.peer_visibility(), Some(PeerVisibility::Hidden));
}

/// GOLDEN-VECTOR: the opt-in flag seam. A published acceptor advertises
/// `DFLAG_PUBLISHED` and the connecting peer classifies it `normal`; a published
/// client flips the server's view; the default stays hidden throughout.
#[test]
fn opt_in_flags_flip_the_peer_visibility_classification() {
    let published = preferred_flags() | publish_flag(false, true);
    assert_ne!(published, preferred_flags());

    // A published server, a default client.
    let (client, server) = establish_with(Mode::Whole, preferred_flags(), published);
    assert_eq!(client.server_flags(), published);
    assert_eq!(server.server_flags(), published);
    assert_eq!(client.client_flags(), preferred_flags());
    assert_eq!(server.client_flags(), preferred_flags());
    assert_eq!(client.peer_visibility(), Some(PeerVisibility::Normal));
    assert_eq!(server.peer_visibility(), Some(PeerVisibility::Hidden));
    assert_eq!(
        client.handshake().map(|h| h.peer_visibility),
        Some(PeerVisibility::Normal)
    );

    // A published client, a default server (the mirror image).
    let (client, server) = establish_with(Mode::Whole, published, preferred_flags());
    assert_eq!(client.peer_visibility(), Some(PeerVisibility::Hidden));
    assert_eq!(server.peer_visibility(), Some(PeerVisibility::Normal));

    // Before establishment there is no peer to classify.
    let fresh = ConnectionDriver::server(SERVER_NAME, COOKIE, SERVER_CREATION);
    assert_eq!(fresh.peer_visibility(), None);

    // The opt-in adds exactly the published bit and moves nothing else.
    assert_eq!(published.bits() & !DFLAG_PUBLISHED, preferred_flags().bits());
}

#[test]
fn handshake_completes_byte_at_a_time_and_a_message_still_flows() {
    let (mut client, mut server) = establish(Mode::ByteAtATime);
    assert_eq!(client.state(), DriverState::Established);
    assert_eq!(server.state(), DriverState::Established);

    // A message still round-trips after the byte-at-a-time handshake.
    let mut atoms = AtomTable::new();
    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));
    client.send_control(Dop::Link, &[from, to], &atoms).expect("send");

    // Also feed the established frame one byte at a time.
    for byte in client.take_output() {
        server.feed(&[byte]);
        server.step().expect("server step");
    }

    let mut heap = Heap::new(4096).expect("heap");
    let msg = server.next_message(&mut heap, &mut atoms).expect("decode").expect("a message");
    assert_eq!(msg.dop, Dop::Link);
    assert_eq!(msg.terms, vec![from, to]);
}

// ---------------------------------------------------------------------------
// (b) A round trip with atoms exercises the atom cache.
// ---------------------------------------------------------------------------

#[test]
fn round_trip_with_atoms_exercises_the_atom_cache() {
    let (mut client, mut server) = establish(Mode::Whole);

    let mut client_atoms = AtomTable::new();
    let from = make_atom(client_atoms.intern("from"));
    let cookie = make_atom(client_atoms.intern("cookie"));
    let to_name = make_atom(client_atoms.intern("to_name"));

    // First message: one `New` ref per atom.
    client.send_control(Dop::RegSend, &[from, cookie, to_name], &client_atoms).expect("send 1");
    let out = client.take_output();
    let header = decode_single_frame_header(&out);
    assert_eq!(header.refs.len(), 3, "one ref per atom");
    assert!(header.refs.iter().all(AtomCacheRef::is_new), "first message installs New refs");

    server.feed(&out);
    server.step().expect("server step 1");
    let mut server_atoms = AtomTable::new();
    let mut server_heap = Heap::new(4096).expect("server heap");
    let msg = server
        .next_message(&mut server_heap, &mut server_atoms)
        .expect("decode 1")
        .expect("a message");
    assert_eq!(msg.dop, Dop::RegSend);
    let names: Vec<String> = msg
        .terms
        .iter()
        .map(|term| server_atoms.name(atom_val(*term)).unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["from", "cookie", "to_name"]);

    // Second message: the same atoms now travel as `Cached` refs, which the
    // server's persistent cache must still resolve.
    client.send_control(Dop::RegSend, &[from, cookie, to_name], &client_atoms).expect("send 2");
    let out = client.take_output();
    let header = decode_single_frame_header(&out);
    assert_eq!(header.refs.len(), 3);
    assert!(header.refs.iter().all(|r| !r.is_new()), "second message reuses Cached refs");

    server.feed(&out);
    server.step().expect("server step 2");
    let msg = server
        .next_message(&mut server_heap, &mut server_atoms)
        .expect("decode 2")
        .expect("a message");
    assert_eq!(msg.dop, Dop::RegSend);
    let names: Vec<String> = msg
        .terms
        .iter()
        .map(|term| server_atoms.name(atom_val(*term)).unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["from", "cookie", "to_name"]);
}

#[test]
fn an_atom_free_message_carries_the_exact_empty_header() {
    let (mut client, mut server) = establish(Mode::Whole);
    let atoms = AtomTable::new();

    // `DOP_SEND` with two integer control args and an integer message: no atoms.
    client
        .send_control(Dop::Send, &[make_small(1), make_small(2), make_small(3)], &atoms)
        .expect("send");
    let out = client.take_output();
    assert_eq!(
        &out[4..7],
        &[beam_codec::VERSION_MAGIC, DIST_HEADER, 0],
        "an atom-free message carries the exact empty header 131, 68, 0"
    );

    server.feed(&out);
    server.step().expect("server step");
    let mut server_atoms = AtomTable::new();
    let mut server_heap = Heap::new(4096).expect("server heap");
    let msg = server
        .next_message(&mut server_heap, &mut server_atoms)
        .expect("decode")
        .expect("a message");
    assert_eq!(msg.dop, Dop::Send);
    assert_eq!(msg.terms, vec![make_small(1), make_small(2), make_small(3)]);
}

// ---------------------------------------------------------------------------
// (c) A message larger than a fragment round-trips both ways.
// ---------------------------------------------------------------------------

#[test]
fn a_message_larger_than_a_fragment_round_trips_both_ways() {
    let (mut client, mut server) = establish(Mode::Whole);

    let mut atoms = AtomTable::new();
    let mut client_heap = Heap::new(1 << 17).expect("client heap");
    let mut server_heap = Heap::new(1 << 17).expect("server heap");
    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));

    // Each list element encodes to 5 bytes, so 14_000 elements exceed 64 KiB.
    let big = big_list(&mut client_heap, 14_000, 1_000);
    let image = body_image(big, &atoms);
    assert!(image.len() > DIST_FRAGMENT_SIZE, "the payload must exceed one fragment");

    // Client -> server.
    client.send_message(from, to, big, &atoms).expect("client sends big");
    let out = client.take_output();
    // The first frame is an `'E'` fragment header (`VERSION_MAGIC 'E'`).
    assert_eq!(out[4], beam_codec::VERSION_MAGIC);
    assert_eq!(out[5], DIST_FRAG_HEADER);
    server.feed(&out);
    server.step().expect("server step");
    let msg = server
        .next_message(&mut server_heap, &mut atoms)
        .expect("decode")
        .expect("big message");
    assert_eq!(msg.dop, Dop::SendSender);
    assert_eq!(body_image(msg.terms[2], &atoms), image);

    // Server -> client (the server fragments independently).
    let back = big_list(&mut server_heap, 14_000, 1_000);
    let back_image = body_image(back, &atoms);
    server.send_message(to, from, back, &atoms).expect("server sends big reply");
    let out = server.take_output();
    assert_eq!(out[4], beam_codec::VERSION_MAGIC);
    assert_eq!(out[5], DIST_FRAG_HEADER);
    client.feed(&out);
    client.step().expect("client step");
    let msg = client
        .next_message(&mut client_heap, &mut atoms)
        .expect("decode reply")
        .expect("big reply");
    assert_eq!(msg.dop, Dop::SendSender);
    assert_eq!(body_image(msg.terms[2], &atoms), back_image);
}

// ---------------------------------------------------------------------------
// (d) A tick (a zero-length packet) is answered with a tick.
// ---------------------------------------------------------------------------

#[test]
fn a_tick_is_answered_with_a_tick() {
    let (_client, mut server) = establish(Mode::Whole);

    // The peer writes a raw tick (a zero-length 4-byte-framed packet).
    server.feed(&[0, 0, 0, 0]);
    assert_eq!(server.step().expect("server step"), StepOutcome::Progress);
    assert_eq!(server.ticks_answered(), 1, "the tick was answered");
    assert_eq!(server.take_output(), vec![0, 0, 0, 0], "a tick answer is queued");
    assert!(!server.has_output(), "take_output drained the buffer");
}

#[test]
fn send_tick_queues_a_zero_length_packet_the_peer_answers() {
    let (mut client, mut server) = establish(Mode::Whole);

    client.send_tick().expect("send tick");
    let out = client.take_output();
    assert_eq!(out, vec![0, 0, 0, 0], "a tick is a zero-length 4-byte-framed packet");

    server.feed(&out);
    let outcome = server.step().expect("server step");
    assert_eq!(outcome, StepOutcome::Progress);
    assert_eq!(server.ticks_answered(), 1);
    assert_eq!(server.take_output(), vec![0, 0, 0, 0], "the peer answers in kind");
}

// ---------------------------------------------------------------------------
// (e) Malformed input is a typed error, never a panic.
// ---------------------------------------------------------------------------

#[test]
fn a_garbage_packet_body_is_a_typed_error() {
    let (_client, mut server) = establish(Mode::Whole);

    // A 4-byte-framed packet whose body starts with a byte that is neither
    // `VERSION_MAGIC` nor `PASS_THROUGH`.
    let mut frame = vec![0, 0, 0, 3];
    frame.extend_from_slice(&[0xAB, 0xCD, 0xEF]);
    server.feed(&frame);

    let err = server.step().expect_err("garbage must fail");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    assert_eq!(server.state(), DriverState::Failed);
    // A failed link stays failed and reports it rather than panicking.
    assert_eq!(server.step().expect("step after failure"), StepOutcome::Failed);
}

#[test]
fn a_truncated_frame_is_a_typed_error() {
    let (_client, mut server) = establish(Mode::Whole);

    // A 4-byte length prefix advertising 8 bytes, but only 3 body bytes, then EOF.
    let mut frame = vec![0, 0, 0, 8];
    frame.extend_from_slice(&[131, 68, 0]);
    server.feed(&frame);
    assert_eq!(server.step().expect("needs more bytes"), StepOutcome::WouldBlock);
    assert_eq!(server.state(), DriverState::Established);

    server.feed_eof();
    let err = server.step().expect_err("a truncated frame must fail at EOF");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    assert_eq!(server.state(), DriverState::Failed);
}

#[test]
fn a_malformed_handshake_frame_is_a_typed_error() {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);

    // The client emits its `Name` frame first.
    let outcome = client.step().expect("client step");
    assert_eq!(outcome, StepOutcome::Handshaking);
    assert!(!client.take_output().is_empty(), "the client sent its Name frame");

    // An empty 2-byte-framed packet, where a `Status` message is expected.
    client.feed(&[0, 0]);
    let err = client.step().expect_err("an empty status frame must fail");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    assert_eq!(client.state(), DriverState::Failed);
}

#[test]
fn an_eof_before_a_handshake_completes_reports_closed() {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
    client.step().expect("client step");
    client.take_output();

    client.feed_eof();
    assert_eq!(client.step().expect("step at EOF"), StepOutcome::Closed);
    assert_eq!(client.state(), DriverState::Closed);
    assert_eq!(client.step().expect("step after close"), StepOutcome::Closed);
    assert!(client.is_eof());
}

#[test]
fn send_control_before_establishment_is_rejected() {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
    let atoms = AtomTable::new();
    let err = client.send_control(Dop::Link, &[], &atoms).expect_err("not established");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

// ---------------------------------------------------------------------------
// (f) The handshake ERROR paths: a refusing acceptor notifies the peer and both
//     refusal directions produce the same typed error as the blocking API.
// ---------------------------------------------------------------------------

/// Encodes a handshake message body through its own codec (a `MemoryTransport`
/// scratch sink, exactly as the driver's own helper does).
fn handshake_body<F>(encode: F) -> Vec<u8>
where
    F: FnOnce(&mut MemoryTransport) -> beam_common::Result<()>,
{
    let mut sink = MemoryTransport::new();
    encode(&mut sink).expect("encode a valid handshake body");
    sink.into_written()
}

/// Wraps a handshake message body in the 2-byte handshake framing.
fn frame_handshake(body: &[u8]) -> Vec<u8> {
    let mut sink = MemoryTransport::new();
    write_packet(&mut sink, PacketHeaderSize::Two, body).expect("frame a handshake body");
    sink.into_written()
}

/// A `Name` message body advertising `flags`.
fn name_body(flags: DistFlags) -> Vec<u8> {
    handshake_body(|w| Name { flags, creation: 0, name: CLIENT_NAME.to_vec() }.encode(w))
}

#[test]
fn a_refusing_acceptor_driver_buffers_not_allowed_for_the_peer() {
    let mut server = ConnectionDriver::server(SERVER_NAME, COOKIE, SERVER_CREATION);

    // A `Name` whose flag word is empty fails the mandatory check (the v6 version
    // gate); a real accepting node replies `not_allowed` and drops the link
    // (`lib/kernel/src/dist_util.erl:286`: `check_dflags` ->
    // `send_status(HSData, not_allowed)`).
    server.feed(&frame_handshake(&name_body(DistFlags::EMPTY)));

    let err = server.step().expect_err("an empty flag word must be refused");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    assert_eq!(server.state(), DriverState::Failed);

    // The refusal bytes were buffered *before* `step` returned the error, so they
    // survive into `take_output`: a 2-byte-framed `Status::not_allowed`, i.e. the
    // wire text `snot_allowed` (12 bytes) inside a 2-byte frame prefix.
    let out = server.take_output();
    assert_eq!(out.len(), 14, "2-byte length prefix + the 12-byte `snot_allowed` body");
    assert_eq!(&out[..2], &[0, 12], "the status body is 12 bytes");
    assert_eq!(&out[2..], b"snot_allowed", "the wire text of Status::not_allowed()");
    let status = Status::decode(&out[2..]).expect("decode the buffered refusal");
    assert_eq!(status, Status::not_allowed());
    assert_eq!(status.kind(), StatusKind::NotAllowed);

    // The rejected link is terminal: it stays failed and never resumes.
    assert_eq!(server.step().expect("step after failure"), StepOutcome::Failed);
    assert_eq!(server.state(), DriverState::Failed);
}

#[test]
fn a_driver_client_classifies_a_peer_refusal_like_the_blocking_client() {
    use std::io::ErrorKind;

    let cases = [
        (Status::nok(), ErrorKind::ConnectionRefused),
        (Status::not_allowed(), ErrorKind::PermissionDenied),
        (Status::Error(b"alive".to_vec()), ErrorKind::ConnectionAborted),
        (
            Status::Named { name: b"taken@host".to_vec(), creation: 5 },
            ErrorKind::AlreadyExists,
        ),
        (Status::Simultaneous, ErrorKind::ConnectionAborted),
        (Status::Error(b"weird".to_vec()), ErrorKind::InvalidData),
    ];

    for (status, expected) in cases {
        // The driver's own classification must be exactly the shared mapping's.
        match status_error(&status) {
            BeamError::Io(io) => assert_eq!(io.kind(), expected, "shared mapping for {status:?}"),
            other => panic!("the shared mapping must be an Io error, got {other:?}"),
        }

        let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
        // The client emits its `Name` frame first; drain it, then answer.
        client.step().expect("client emits its Name");
        client.take_output();
        client.feed(&frame_handshake(&handshake_body(|w| status.encode(w))));

        let err = client.step().expect_err("a non-ok status must fail the client");
        match err {
            BeamError::Io(io) => assert_eq!(io.kind(), expected, "driver refusal {status:?}"),
            other => panic!("driver must return an Io error for {status:?}, got {other:?}"),
        }
        assert_eq!(client.state(), DriverState::Failed, "refusal is terminal ({status:?})");
        assert_eq!(client.step().expect("step after failure"), StepOutcome::Failed);
    }
}

#[test]
fn a_driver_client_truncated_status_frame_ends_failed_at_eof_not_wouldblock() {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
    client.step().expect("client emits its Name");
    client.take_output();

    // A 2-byte frame advertising 5 body bytes, but only 2 arrive — so without EOF
    // the driver honestly reports `WouldBlock` (it needs more input).
    client.feed(&[0, 5, b's', b'n']);
    assert_eq!(client.step().expect("needs more bytes"), StepOutcome::WouldBlock);
    assert_eq!(client.state(), DriverState::Handshaking);

    // At EOF the partial frame is a truncated message: a typed error, terminal.
    client.feed_eof();
    let err = client.step().expect_err("a truncated status frame must fail at EOF");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    assert_eq!(client.state(), DriverState::Failed);
    assert_eq!(client.step().expect("step after failure"), StepOutcome::Failed);
}
