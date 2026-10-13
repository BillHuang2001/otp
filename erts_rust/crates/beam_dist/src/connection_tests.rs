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

//! Path-included integration-style tests for [`super`] (`connection`).
//!
//! Kept in a sibling file so `connection.rs` stays under the ~1000-line
//! convention; included via `#[path]` so `super::*` still reaches the private
//! helpers of the `connection` module.

use super::*;
use crate::control::encode_control;
use crate::dist_header::AtomCacheRef;
use crate::transport::MemoryTransport;
use beam_codec::{AtomEncoding, Encoder, ATOM_CACHE_REF_EXT, SMALL_TUPLE_EXT};
use beam_term::prelude::{make_atom, make_small, NIL};

const COOKIE: &[u8] = b"a-test-cookie";
const CLIENT: &[u8] = b"client@host";
const SERVER: &[u8] = b"server@host";

/// Frames `body` with the given length prefix.
fn framed(body: &[u8], hdr: PacketHeaderSize) -> Vec<u8> {
    let mut sink = MemoryTransport::new();
    write_packet(&mut sink, hdr, body).expect("frame");
    sink.into_written()
}

/// Encodes a message body through its own codec.
fn encoded<F>(encode: F) -> Vec<u8>
where
    F: FnOnce(&mut MemoryTransport) -> Result<()>,
{
    let mut sink = MemoryTransport::new();
    encode(&mut sink).expect("encode");
    sink.into_written()
}

/// Reads one handshake frame out of a byte reader.
fn read_handshake_frame(reader: &mut MemoryTransport) -> Vec<u8> {
    read_packet(reader, HANDSHAKE_FRAMING).expect("read frame")
}

// -----------------------------------------------------------------------
// connect_client
// -----------------------------------------------------------------------

/// The scripted server half of a successful client handshake.
fn client_script(our_challenge: u32, peer_challenge: u32, creation: u32) -> Vec<u8> {
    let server_flags = preferred_flags();
    let mut input = Vec::new();
    input.extend(framed(&encoded(|w| Status::Ok.encode(w)), HANDSHAKE_FRAMING));
    input.extend(framed(
        &encoded(|w| {
            Challenge {
                flags: server_flags,
                challenge: peer_challenge,
                creation,
                name: SERVER.to_vec(),
            }
            .encode(w)
        }),
        HANDSHAKE_FRAMING,
    ));
    input.extend(framed(
        &encoded(|w| ChallengeAck { digest: gen_digest(COOKIE, our_challenge) }.encode(w)),
        HANDSHAKE_FRAMING,
    ));
    input
}

#[test]
fn client_drives_the_full_handshake() {
    let our_challenge = 0x1234_5678;
    let peer_challenge = 0xdead_beef;
    let mut transport =
        MemoryTransport::with_input(client_script(our_challenge, peer_challenge, 41));

    let hs = connect_client_with(&mut transport, CLIENT, COOKIE, our_challenge)
        .expect("handshake");

    assert_eq!(hs.client_flags, preferred_flags());
    assert_eq!(hs.server_flags, preferred_flags());
    assert_eq!(hs.negotiated, check_mandatory(preferred_flags()).unwrap());
    assert_eq!(hs.peer_creation, 41);
    assert_eq!(hs.peer_name, SERVER);
    assert_eq!(hs.buffered, None);

    // The client sent Name then ChallengeReply, both 2-byte framed.
    let mut output = MemoryTransport::with_input(transport.into_written());
    assert_eq!(
        Name::decode(&read_handshake_frame(&mut output)).unwrap(),
        Name { flags: preferred_flags(), creation: CLIENT_CREATION, name: CLIENT.to_vec() }
    );
    let reply = ChallengeReply::decode(&read_handshake_frame(&mut output)).unwrap();
    assert_eq!(reply.challenge, our_challenge);
    assert_eq!(reply.digest, gen_digest(COOKIE, peer_challenge));
    assert_eq!(output.input_remaining(), 0);
}

#[test]
fn connect_client_with_creation_advertises_the_creation() {
    // A peer that answers the `Name` with `Status::Ok` only. The handshake then
    // fails for lack of further input, but the `Name` frame written first is on
    // the wire for inspection (the nonce is random, so a scripted `ChallengeAck`
    // cannot be precomputed).
    let mut transport = MemoryTransport::with_input(framed(
        &encoded(|w| Status::Ok.encode(w)),
        HANDSHAKE_FRAMING,
    ));
    let outcome = connect_client_with_creation(&mut transport, CLIENT, COOKIE, 42);
    assert!(outcome.is_err(), "a truncated script cannot complete the handshake");

    let mut output = MemoryTransport::with_input(transport.into_written());
    let name = Name::decode(&read_handshake_frame(&mut output)).unwrap();
    assert_eq!(name.creation, 42, "the requested creation is advertised");
    assert_eq!(name, Name { flags: preferred_flags(), creation: 42, name: CLIENT.to_vec() });
}

#[test]
fn client_leaves_a_pipelined_packet_for_the_connection() {
    let our_challenge = 0x0abc_def0;
    let mut input = client_script(our_challenge, 7, 3);
    // A peer that pipelines its first real packet right after the ack: the
    // handshake reads no complement, so the frame must stay in the transport
    // for the established `Connection` to consume.
    input.extend(framed(&[1, 2, 3], HANDSHAKE_FRAMING));

    let mut transport = MemoryTransport::with_input(input);
    let hs = connect_client_with(&mut transport, CLIENT, COOKIE, our_challenge)
        .expect("handshake");
    assert_eq!(hs.buffered, None);
    assert!(
        transport.input_remaining() > 0,
        "the handshake must not consume the pipelined packet"
    );
}

#[test]
fn client_rejects_a_refused_status() {
    let input = framed(&encoded(|w| Status::Error(b"nok".to_vec()).encode(w)), HANDSHAKE_FRAMING);
    let mut transport = MemoryTransport::with_input(input);
    let err = connect_client_with(&mut transport, CLIENT, COOKIE, 1).unwrap_err();
    match err {
        BeamError::Io(io) => {
            assert_eq!(io.kind(), std::io::ErrorKind::ConnectionRefused);
            assert!(io.to_string().contains("nok"), "the status text is carried: {io}");
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn client_rejects_a_bad_ack_digest() {
    let our_challenge = 0x5555_aaaa;
    let server_flags = preferred_flags();
    let mut input = Vec::new();
    input.extend(framed(&encoded(|w| Status::Ok.encode(w)), HANDSHAKE_FRAMING));
    input.extend(framed(
        &encoded(|w| {
            Challenge {
                flags: server_flags,
                challenge: 9,
                creation: 2,
                name: SERVER.to_vec(),
            }
            .encode(w)
        }),
        HANDSHAKE_FRAMING,
    ));
    input.extend(framed(
        &encoded(|w| ChallengeAck { digest: [0u8; 16] }.encode(w)),
        HANDSHAKE_FRAMING,
    ));

    let mut transport = MemoryTransport::with_input(input);
    assert!(matches!(
        connect_client_with(&mut transport, CLIENT, COOKIE, our_challenge),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn client_rejects_a_peer_missing_mandatory_flags() {
    let mut input = Vec::new();
    input.extend(framed(&encoded(|w| Status::Ok.encode(w)), HANDSHAKE_FRAMING));
    input.extend(framed(
        &encoded(|w| {
            Challenge {
                flags: DistFlags::EMPTY,
                challenge: 9,
                creation: 2,
                name: SERVER.to_vec(),
            }
            .encode(w)
        }),
        HANDSHAKE_FRAMING,
    ));
    let mut transport = MemoryTransport::with_input(input);
    assert!(matches!(
        connect_client_with(&mut transport, CLIENT, COOKIE, 1),
        Err(BeamError::BadArg)
    ));
}

// -----------------------------------------------------------------------
// accept_server
// -----------------------------------------------------------------------

#[test]
fn server_drives_the_full_handshake() {
    let our_challenge = 0x0bad_f00d;
    let peer_challenge = 0x0102_0304;
    let mut input = Vec::new();
    input.extend(framed(
        &encoded(|w| {
            Name { flags: preferred_flags(), creation: 9, name: CLIENT.to_vec() }.encode(w)
        }),
        HANDSHAKE_FRAMING,
    ));
    input.extend(framed(
        &encoded(|w| {
            ChallengeReply {
                challenge: peer_challenge,
                digest: gen_digest(COOKIE, our_challenge),
            }
            .encode(w)
        }),
        HANDSHAKE_FRAMING,
    ));

    let mut transport = MemoryTransport::with_input(input);
    let hs = accept_server_with(&mut transport, SERVER, COOKIE, 77, our_challenge)
        .expect("handshake");

    assert_eq!(hs.client_flags, preferred_flags());
    assert_eq!(hs.server_flags, preferred_flags());
    assert_eq!(hs.negotiated, check_mandatory(preferred_flags()).unwrap());
    assert_eq!(hs.peer_creation, 9);
    assert_eq!(hs.peer_name, CLIENT);
    assert_eq!(hs.buffered, None);

    // The server sent Status::Ok, Challenge and ChallengeAck — and nothing
    // else (no complement on the `'N'` handshake).
    let mut output = MemoryTransport::with_input(transport.into_written());
    assert_eq!(Status::decode(&read_handshake_frame(&mut output)).unwrap(), Status::Ok);
    assert_eq!(
        Challenge::decode(&read_handshake_frame(&mut output)).unwrap(),
        Challenge {
            flags: preferred_flags(),
            challenge: our_challenge,
            creation: 77,
            name: SERVER.to_vec(),
        }
    );
    assert_eq!(
        ChallengeAck::decode(&read_handshake_frame(&mut output)).unwrap().digest,
        gen_digest(COOKIE, peer_challenge)
    );
    assert_eq!(output.input_remaining(), 0);
}

#[test]
fn server_rejects_a_bad_reply_digest() {
    let mut input =
        framed(&encoded(|w| Name { flags: preferred_flags(), creation: 9, name: CLIENT.to_vec() }.encode(w)), HANDSHAKE_FRAMING);
    input.extend(framed(
        &encoded(|w| ChallengeReply { challenge: 3, digest: [0xffu8; 16] }.encode(w)),
        HANDSHAKE_FRAMING,
    ));
    let mut transport = MemoryTransport::with_input(input);
    assert!(matches!(
        accept_server_with(&mut transport, SERVER, COOKIE, 1, 42),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn server_refuses_bad_flags_and_tells_the_peer() {
    // Both a totally empty flag word and one that is missing just the OTP-23+
    // handshake marker (`DFLAG_HANDSHAKE_23`, the v6 version gate inside
    // `DFLAG_DIST_MANDATORY`) fail `check_mandatory`.
    let bad_flag_words = [
        DistFlags::EMPTY,
        DistFlags::from_bits(crate::flags::DFLAG_DIST_MANDATORY & !crate::flags::DFLAG_HANDSHAKE_23),
    ];
    for flags in bad_flag_words {
        let mut input = framed(
            &encoded(|w| Name { flags, creation: 3, name: CLIENT.to_vec() }.encode(w)),
            HANDSHAKE_FRAMING,
        );
        // A pipelined `ChallengeReply` must never be read: the server stops after
        // the `Name` it refused.
        input.extend(framed(
            &encoded(|w| ChallengeReply { challenge: 1, digest: [0u8; 16] }.encode(w)),
            HANDSHAKE_FRAMING,
        ));

        let mut transport = MemoryTransport::with_input(input);
        let err = accept_server_with(&mut transport, SERVER, COOKIE, 1, 42).unwrap_err();
        assert!(
            matches!(err, BeamError::BadArg),
            "the refusal is the mandatory-flag error for {flags:?}: {err:?}"
        );

        // The peer is notified *before* the function returns: exactly one frame, the
        // `not_allowed` status a real accepting node sends (`dist_util.erl:286`).
        let mut output = MemoryTransport::with_input(transport.into_written());
        let frame = read_handshake_frame(&mut output);
        assert_eq!(frame, b"snot_allowed", "for flags {flags:?}");
        let decoded = Status::decode(&frame).unwrap();
        assert_eq!(decoded, Status::not_allowed());
        assert_eq!(decoded.kind(), StatusKind::NotAllowed);
        assert_eq!(decoded.error_kind(), Some(StatusKind::NotAllowed));
        assert_eq!(output.input_remaining(), 0, "the refusal is the only frame written");
    }
}

#[test]
fn server_does_not_notify_the_peer_on_a_bad_digest() {
    // Ground truth: `recv_challenge_reply` on a bad cookie shuts the link down
    // *without* a status (`dist_util.erl:1006`), so the acceptor must not invent
    // one either — the only refusal it announces is the flag-gate one.
    let mut input = framed(
        &encoded(|w| {
            Name { flags: preferred_flags(), creation: 9, name: CLIENT.to_vec() }.encode(w)
        }),
        HANDSHAKE_FRAMING,
    );
    input.extend(framed(
        &encoded(|w| ChallengeReply { challenge: 3, digest: [0xffu8; 16] }.encode(w)),
        HANDSHAKE_FRAMING,
    ));
    let mut transport = MemoryTransport::with_input(input);
    assert!(accept_server_with(&mut transport, SERVER, COOKIE, 1, 42).is_err());
    // `Status::Ok` + `Challenge` were written first; a third frame would be a status.
    let mut output = MemoryTransport::with_input(transport.into_written());
    assert_eq!(Status::decode(&read_handshake_frame(&mut output)).unwrap(), Status::Ok);
    assert!(Challenge::decode(&read_handshake_frame(&mut output)).is_ok());
    assert_eq!(output.input_remaining(), 0, "no status is sent after a bad digest");
}

#[test]
fn server_rejects_a_non_name_first_frame_without_a_status() {
    // A first frame whose tag is not `'N'` is dropped silently, matching
    // `dist_util.erl:749-750` (`recv_name` → `?shutdown2({no_node, Socket}, …)`).
    let mut transport =
        MemoryTransport::with_input(framed(&[b'Z', 1, 2, 3, 4, 5, 6, 7, 8], HANDSHAKE_FRAMING));
    assert!(matches!(
        accept_server_with(&mut transport, SERVER, COOKIE, 1, 42),
        Err(BeamError::BadArg)
    ));
    assert!(transport.into_written().is_empty(), "nothing is written for a bogus tag");
}

#[test]
fn client_maps_every_status_kind_to_a_typed_error() {
    let cases: [(Status, std::io::ErrorKind, &str); 6] = [
        (Status::nok(), std::io::ErrorKind::ConnectionRefused, "nok"),
        (Status::not_allowed(), std::io::ErrorKind::PermissionDenied, "not_allowed"),
        (Status::Error(b"alive".to_vec()), std::io::ErrorKind::ConnectionAborted, "alive"),
        (
            Status::Named { name: SERVER.to_vec(), creation: 7 },
            std::io::ErrorKind::AlreadyExists,
            "named",
        ),
        (Status::Simultaneous, std::io::ErrorKind::ConnectionAborted, "ok_simultaneous"),
        (Status::Error(b"what_is_this".to_vec()), std::io::ErrorKind::InvalidData, "what_is_this"),
    ];
    for (status, kind, needle) in cases {
        let input = framed(&encoded(|w| status.encode(w)), HANDSHAKE_FRAMING);
        let mut transport = MemoryTransport::with_input(input);
        match connect_client_with(&mut transport, CLIENT, COOKIE, 1).unwrap_err() {
            BeamError::Io(io) => {
                assert_eq!(io.kind(), kind, "wrong io kind for {status:?}");
                assert!(
                    io.to_string().contains(needle),
                    "the message must name the status ({needle}): {io}"
                );
            }
            other => panic!("expected Io for {status:?}, got {other:?}"),
        }
    }
}

#[test]
fn client_rejects_a_bogus_status_tag() {
    // A status frame whose tag is not `'s'` is protocol garbage: `BadArg`, promptly.
    let mut transport = MemoryTransport::with_input(framed(&[b'Z', b'o', b'k'], HANDSHAKE_FRAMING));
    assert!(matches!(
        connect_client_with(&mut transport, CLIENT, COOKIE, 1),
        Err(BeamError::BadArg)
    ));
}
// -----------------------------------------------------------------------
// Established-connection framing: header, pass-through, tick, fragments.
// -----------------------------------------------------------------------

/// A connection over a memory transport whose input is pre-loaded, with a
/// synthetic completed handshake.
fn connection(input: Vec<u8>) -> Connection<MemoryTransport> {
    let flags = preferred_flags();
    Connection::from_handshake(
        MemoryTransport::with_input(input),
        Handshake {
            client_flags: flags,
            server_flags: flags,
            negotiated: flags,
            peer_creation: 0,
            peer_name: Vec::new(),
            peer_visibility: PeerVisibility::Hidden,
            buffered: None,
        },
        b"local@host",
    )
}

/// The established-packet body for `dop`/`terms`: the empty distribution
/// header (`131, 'D', 0`) then the real `{DOP_*, …}` tuple image.
fn packet_body(dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Vec<u8> {
    let mut body = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0];
    body.extend_from_slice(&encode_control(dop, terms, atoms).expect("encode the tuple"));
    body
}

#[test]
fn send_control_emits_the_real_tuple_wire_form() {
    let atoms = AtomTable::new();
    let mut conn = connection(Vec::new());
    conn.send_control(Dop::Link, &[make_small(1), make_small(2)], &atoms).unwrap();

    let mut written = MemoryTransport::with_input(conn.into_inner().into_written());
    let body = read_packet(&mut written, ESTABLISHED_FRAMING).unwrap();
    // The distribution header, then the control tuple `{DOP_LINK, 1, 2}`.
    assert_eq!(&body[..3], &[beam_codec::VERSION_MAGIC, DIST_HEADER, 0]);
    assert_eq!(body[3], SMALL_TUPLE_EXT, "the control message must be a tuple");
    assert_eq!(body[4], 3, "`{{DOP_LINK, A, B}}` has three elements");
    assert_eq!(body[5], beam_codec::SMALL_INTEGER_EXT, "the opcode is a small-integer term");
    assert_eq!(body[6], Dop::Link.as_u8());
}

#[test]
fn an_atom_free_message_still_emits_the_empty_header() {
    // No atom in the body => no atom-cache ref => the exact bytes `131, 'D', 0`.
    let atoms = AtomTable::new();
    let mut conn = connection(Vec::new());
    conn.send_control(Dop::Link, &[make_small(1), make_small(2)], &atoms).unwrap();

    let mut written = MemoryTransport::with_input(conn.into_inner().into_written());
    let body = read_packet(&mut written, ESTABLISHED_FRAMING).unwrap();
    assert_eq!(&body[..3], &[beam_codec::VERSION_MAGIC, DIST_HEADER, 0]);
}

#[test]
fn the_outbound_atom_cache_round_trips_header_and_payload() {
    // Real atoms, interned in a real table.
    let mut atoms = AtomTable::new();
    let from = make_atom(atoms.intern("from"));
    let cookie = make_atom(atoms.intern("cookie"));
    let to_name = make_atom(atoms.intern("to_name"));

    // Encode `{DOP_REG_SEND, From, Cookie, ToName}` and read the emitted packet.
    let mut sender = connection(Vec::new());
    sender.send_control(Dop::RegSend, &[from, cookie, to_name], &atoms).unwrap();
    let mut reader = MemoryTransport::with_input(sender.into_inner().into_written());
    let body = read_packet(&mut reader, ESTABLISHED_FRAMING).unwrap();

    // The header teaches one `New` ref per atom, in payload order.
    let mut header_atoms = AtomTable::new();
    let (header, offset) =
        DistHeader::decode_tail(&body, 2, &mut header_atoms, AtomEncoding::Utf8).unwrap();
    assert_eq!(header.refs.len(), 3);
    assert!(
        header.refs.iter().all(AtomCacheRef::is_new),
        "the first message teaches every atom"
    );
    let ref_name = |r: &AtomCacheRef| match r {
        AtomCacheRef::New { atom, .. } => header_atoms.name(*atom).unwrap().to_string(),
        AtomCacheRef::Cached { .. } => panic!("expected a New ref"),
    };
    assert_eq!(
        header.refs.iter().map(ref_name).collect::<Vec<_>>(),
        vec!["from", "cookie", "to_name"],
    );

    // The payload writes `82, index` for each atom: tuple arity 4 (opcode + 3 args).
    let payload = &body[offset..];
    assert_eq!(
        payload,
        &[
            SMALL_TUPLE_EXT,
            4,
            beam_codec::SMALL_INTEGER_EXT,
            Dop::RegSend.as_u8(),
            ATOM_CACHE_REF_EXT,
            0,
            ATOM_CACHE_REF_EXT,
            1,
            ATOM_CACHE_REF_EXT,
            2,
        ]
    );

    // Feed the exact bytes into a receiving `Connection`: header refs and
    // payload indices agree iff each `82, n` resolves to header ref `n`.
    let mut receiver = connection(framed(&body, ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut rx_atoms = AtomTable::new();
    let msg = receiver.recv(&mut heap, &mut rx_atoms).unwrap();
    assert_eq!(msg.dop, Dop::RegSend);
    assert_eq!(
        msg.terms,
        vec![
            make_atom(rx_atoms.lookup("from").unwrap()),
            make_atom(rx_atoms.lookup("cookie").unwrap()),
            make_atom(rx_atoms.lookup("to_name").unwrap()),
        ]
    );
}

#[test]
fn a_second_message_re_uses_cached_refs() {
    let mut atoms = AtomTable::new();
    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));

    let mut conn = connection(Vec::new());
    conn.send_control(Dop::Link, &[from, to], &atoms).unwrap();
    conn.send_control(Dop::Link, &[from, to], &atoms).unwrap();
    let mut reader = MemoryTransport::with_input(conn.into_inner().into_written());
    let first = read_packet(&mut reader, ESTABLISHED_FRAMING).unwrap();
    let second = read_packet(&mut reader, ESTABLISHED_FRAMING).unwrap();

    let mut scratch = AtomTable::new();
    let (h1, _) = DistHeader::decode_tail(&first, 2, &mut scratch, AtomEncoding::Utf8).unwrap();
    let (h2, _) =
        DistHeader::decode_tail(&second, 2, &mut scratch, AtomEncoding::Utf8).unwrap();
    assert_eq!(h1.refs.len(), 2);
    assert!(h1.refs.iter().all(AtomCacheRef::is_new));
    // The persistent cache survived `take_header`: the second message re-uses it.
    assert_eq!(h2.refs.len(), 2);
    assert!(h2.refs.iter().all(|r| !r.is_new()), "the second message must be cached-only");
    assert_eq!(
        h2.refs.iter().map(|r| (r.segment(), r.index())).collect::<Vec<_>>(),
        h1.refs.iter().map(|r| (r.segment(), r.index())).collect::<Vec<_>>(),
        "the cached refs address the slots the first message installed",
    );
}
#[test]
fn recv_decodes_a_normal_distribution_header_packet() {
    let atoms = AtomTable::new();
    let body = packet_body(Dop::Link, &[make_small(1), make_small(2)], &atoms);

    let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    let msg = conn.recv(&mut heap, &mut atoms).unwrap();
    assert_eq!(msg.dop, Dop::Link);
    assert_eq!(msg.terms, vec![make_small(1), make_small(2)]);
}

#[test]
fn recv_accepts_the_pass_through_form() {
    let atoms = AtomTable::new();
    let mut body = vec![PASS_THROUGH];
    body.extend_from_slice(
        &encode_control(Dop::Link, &[make_small(3), make_small(4)], &atoms).unwrap(),
    );

    let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    let msg = conn.recv(&mut heap, &mut atoms).unwrap();
    assert_eq!(msg.dop, Dop::Link);
    assert_eq!(msg.terms, vec![make_small(3), make_small(4)]);
}

#[test]
fn recv_answers_a_tick_and_delivers_the_next_message() {
    let atoms = AtomTable::new();
    let body = packet_body(Dop::Link, &[make_small(5), make_small(6)], &atoms);

    let mut input = framed(&[], ESTABLISHED_FRAMING); // the tick
    input.extend(framed(&body, ESTABLISHED_FRAMING));
    let mut conn = connection(input);

    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    let msg = conn.recv(&mut heap, &mut atoms).unwrap();
    assert_eq!(msg.terms, vec![make_small(5), make_small(6)]);
    assert_eq!(conn.ticks_answered(), 1);

    // The answer is a zero-length packet.
    let mut written = MemoryTransport::with_input(conn.into_inner().into_written());
    assert!(is_tick(&read_packet(&mut written, ESTABLISHED_FRAMING).unwrap()));
    assert_eq!(written.input_remaining(), 0);
}

#[test]
fn recv_reassembles_a_fragment_sequence() {
    let atoms = AtomTable::new();
    let control =
        encode_control(Dop::Link, &[make_small(7), make_small(8)], &atoms).unwrap();
    let frames = split(3, &[0], &control, 4).unwrap();
    assert!(frames.len() > 1, "the payload must span fragments");

    let mut input = Vec::new();
    for frame in &frames {
        input.extend(framed(frame, ESTABLISHED_FRAMING));
    }
    let mut conn = connection(input);
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    let msg = conn.recv(&mut heap, &mut atoms).unwrap();
    assert_eq!(msg.dop, Dop::Link);
    assert_eq!(msg.terms, vec![make_small(7), make_small(8)]);
}

#[test]
fn recv_rejects_a_message_interleaved_with_a_fragment_sequence() {
    let atoms = AtomTable::new();
    let control =
        encode_control(Dop::Link, &[make_small(1), make_small(2)], &atoms).unwrap();
    let mut payload = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0];
    payload.extend_from_slice(&control);
    let frames = split(4, &[0], &control, 4).unwrap();
    assert!(frames.len() > 1, "the control body must span fragments");

    let mut input = framed(&frames[0], ESTABLISHED_FRAMING);
    input.extend(framed(&payload, ESTABLISHED_FRAMING));
    let mut conn = connection(input);
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}

#[test]
fn recv_rejects_an_unknown_message_shape() {
    let mut conn = connection(framed(&[0x00, 0x00], ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}

#[test]
fn recv_reports_a_clean_peer_eof_at_a_frame_boundary_distinctly() {
    // A peer that closes the connection with nothing buffered has ended the link
    // cleanly: the frame read starts at a boundary and reads no byte at all.
    // That must be a typed `UnexpectedEof`, NOT the `BadArg` reserved for
    // malformed/truncated wire data — so a caller can tell "the peer went away"
    // from "the wire is corrupt".
    let mut conn = connection(Vec::new());
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    match conn.recv(&mut heap, &mut atoms) {
        Err(BeamError::Io(err)) => assert_eq!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "a clean peer EOF must be UnexpectedEof: {err}"
        ),
        other => panic!("a clean peer EOF must be UnexpectedEof, not {other:?}"),
    }
}

#[test]
fn recv_rejects_a_truncated_length_prefix_with_badarg() {
    // Only two of the four length-prefix bytes arrived before the end of the
    // input: the frame was *started*, so this is a truncated frame (BadArg), not
    // a clean EOF.
    let mut conn = connection(vec![0x00, 0x00]);
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}

#[test]
fn recv_rejects_a_truncated_body_with_badarg() {
    // A well-formed length prefix advertising four body bytes, but only two are
    // present: the frame boundary was passed, so this is still a truncated frame.
    let mut input = framed(&[0xff, 0xff, 0xff, 0xff], ESTABLISHED_FRAMING);
    input.truncate(input.len() - 2);
    let mut conn = connection(input);
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}
#[test]
fn send_message_selects_the_negotiated_send_opcode() {
    let atoms = AtomTable::new();
    let mut conn = connection(Vec::new());
    conn.send_message(make_small(1), make_small(2), NIL, &atoms).unwrap();

    let mut written = MemoryTransport::with_input(conn.into_inner().into_written());
    let body = read_packet(&mut written, ESTABLISHED_FRAMING).unwrap();
    assert_eq!(&body[..3], &[beam_codec::VERSION_MAGIC, DIST_HEADER, 0]);
    assert_eq!(body[3], SMALL_TUPLE_EXT, "the control message must be a tuple");
    assert_eq!(body[4], 3, "`{{DOP_SEND_SENDER, From, To}}` has three elements");
    assert_eq!(body[5], beam_codec::SMALL_INTEGER_EXT, "the opcode is a small-integer term");
    assert_eq!(body[6], Dop::SendSender.as_u8());
}

#[test]
fn recv_rejects_a_tuple_with_the_wrong_arity() {
    let atoms = AtomTable::new();
    // `DOP_LINK` takes two control arguments; a third is malformed.
    let body = packet_body(Dop::Link, &[make_small(1), make_small(2), make_small(3)], &atoms);

    let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}

#[test]
fn recv_rejects_a_truncated_control_tuple() {
    // `SMALL_TUPLE_EXT` claims two elements but only the opcode follows.
    let atoms = AtomTable::new();
    let mut encoder = Encoder::new_body(&atoms);
    encoder.encode(make_small(Dop::Link.as_u8() as i64)).unwrap();
    let mut body = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0, SMALL_TUPLE_EXT, 2];
    body.extend_from_slice(&encoder.finish());

    let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}

#[test]
fn recv_rejects_a_non_tuple_control_message() {
    // A body whose control payload is not a tuple (here a bare atom) must be
    // rejected: `dist.c:2205` requires `is_tuple`.
    let mut atoms = AtomTable::new();
    let term = make_atom(atoms.intern("not_a_tuple"));
    let mut encoder = Encoder::new_body(&atoms);
    encoder.encode(term).unwrap();
    let mut body = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0];
    body.extend_from_slice(&encoder.finish());

    let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
    let mut heap = Heap::new(64).unwrap();
    let mut atoms = AtomTable::new();
    assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
}
