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

//! `ALIVE2_REQ`/`ALIVE2_RESP`/`ALIVE2_X_RESP`/`PORT2_REQ`/`PORT2_RESP`/`NAMES`
//! byte-exact codec round-trips and `EpmdClient` request/response over
//! `MemoryTransport`.
//!
//! Every expectation is anchored to the legacy C ground truth:
//! `lib/erl_interface/src/epmd/epmd_publish.c` (client `ALIVE2_REQ`) +
//! `epmd_port.c` (client `PORT2_REQ`) + `ei_epmd.h` (tag values) and
//! `erts/epmd/src/epmd_srv.c` (server layouts, the `'M'`/`'H'` node types, the
//! `name <name> at port <port>\n` NAMES line, the `result`-byte encoding).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_dist::connection::{connect_to_node, Connection, TcpListener as DistListener, TcpTransport};use beam_dist::epmd::{
    lookup, lookup_at, lookup_port, lookup_port_at, Alive2Req, Alive2Resp, DumpLine, DumpNodeKind,
    DumpReq, DumpResp, EpmdClient, KillReq, KillResp, NamesResp, Port2Req, Port2Resp, StopReq,
    StopResp, EPMD_ALIVE2_REQ, EPMD_ALIVE2_RESP, EPMD_ALIVE2_X_RESP, EPMD_DEFAULT_ADDR,
    EPMD_DIST_HIGH, EPMD_DIST_LOW, EPMD_DUMP_REQ, EPMD_KILL_REQ, EPMD_NAMES_REQ, EPMD_NODE_HIDDEN,
    EPMD_NODE_NORMAL, EPMD_PORT2_REQ, EPMD_PORT2_RESP, EPMD_PORT_PLEASE2_REQ, EPMD_PROTO_TCP,
    EPMD_STOP_REQ, MAX_DUMP_RESP_BODY,
};use beam_dist::flags::Dop;
use beam_dist::framing::{write_packet, PacketHeaderSize};
use beam_dist::transport::{MemoryTransport, Read, Write};
use beam_term::prelude::make_atom;
use beam_term::AtomTable;

/// Encodes a message body (`tag + fields`, no length prefix) into bytes.
macro_rules! body {
    ($msg:expr) => {{
        let mut t = MemoryTransport::new();
        $msg.encode(&mut t).expect("encode body");
        t.into_written()
    }};
}

/// Wraps a body in the 2-byte big-endian length frame `epmd` uses.
macro_rules! framed {
    ($body:expr) => {{
        let mut t = MemoryTransport::new();
        write_packet(&mut t, PacketHeaderSize::Two, $body).expect("write_packet");
        t.into_written()
    }};
}

/// A shared [`MemoryTransport`] handle.
///
/// `EpmdClient::new` consumes the transport, so the client's write side is only
/// observable if the test keeps a second handle to the same buffer. This local
/// wrapper delegates to [`MemoryTransport`] and is shared via `Rc<RefCell<..>>`,
/// which lets a test assert both the reply it served and the request bytes the
/// client actually wrote.
#[derive(Clone)]
struct SharedTransport(Rc<RefCell<MemoryTransport>>);

impl SharedTransport {
    /// Wraps a transport (also returned to the caller as a second handle).
    fn new(transport: MemoryTransport) -> (SharedTransport, SharedTransport) {
        let shared = Rc::new(RefCell::new(transport));
        (SharedTransport(Rc::clone(&shared)), SharedTransport(shared))
    }

    /// The bytes written through this handle so far.
    fn written(&self) -> Vec<u8> {
        self.0.borrow().written().to_vec()
    }
}

impl Read for SharedTransport {
    fn read_exact(&mut self, buf: &mut [u8]) -> beam_common::Result<()> {
        self.0.borrow_mut().read_exact(buf)
    }
}

impl Write for SharedTransport {
    fn write_all(&mut self, buf: &[u8]) -> beam_common::Result<()> {
        self.0.borrow_mut().write_all(buf)
    }

    fn flush(&mut self) -> beam_common::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// The canonical `ALIVE2_REQ` used across the tests.
fn gurka_req() -> Alive2Req {
    Alive2Req {
        port: 0x1F90,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: 6,
        low_vsn: 6,
        name: b"gurka".to_vec(),
        extra: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// ALIVE2_REQ
// ---------------------------------------------------------------------------

/// Layout per `epmd_srv.c`: `'x' + port(2) + ntype(1) + proto(1) + high(2) +
/// low(2) + namelen(2) + name + extralen(2) + extra`.
#[test]
fn alive2_req_byte_exact() {
    let expected: Vec<u8> = vec![
        b'x', // EPMD_ALIVE2_REQ (120)
        0x1F, 0x90, // port 0x1F90
        b'M',  // EPMD_NODE_NORMAL (77)
        0x00, // EPMD_PROTO_TCP (0)
        0x00, 0x06, // EI_DIST_HIGH (6)
        0x00, 0x06, // EI_DIST_LOW (6)
        0x00, 0x05, // namelen (5)
        b'g', b'u', b'r', b'k', b'a', // name
        0x00, 0x00, // extralen (0)
    ];
    assert_eq!(body!(&gurka_req()), expected);
    assert_eq!(expected[0], EPMD_ALIVE2_REQ);
    assert_eq!(expected[0], 120);
}

#[test]
fn alive2_req_roundtrip() {
    let req = gurka_req();
    assert_eq!(Alive2Req::decode(&body!(&req)).expect("decode"), req);

    // A non-empty extra field and a hidden node type survive the round-trip.
    let req = Alive2Req { node_type: EPMD_NODE_HIDDEN, extra: b"extra-data".to_vec(), ..gurka_req() };
    assert_eq!(Alive2Req::decode(&body!(&req)).expect("decode"), req);
}

#[test]
fn alive2_req_rejects_wrong_tag_or_empty() {
    let mut raw = body!(&gurka_req());
    raw[0] = EPMD_PORT2_REQ; // 'z' instead of 'x'
    assert!(matches!(Alive2Req::decode(&raw), Err(BeamError::BadArg)));
    assert!(matches!(Alive2Req::decode(&[]), Err(BeamError::BadArg)));
}

#[test]
fn alive2_req_rejects_truncated_body() {
    let raw = body!(&gurka_req());
    // Every proper prefix is a truncated body, never a panic.
    for cut in 0..raw.len() {
        assert!(
            matches!(Alive2Req::decode(&raw[..cut]), Err(BeamError::BadArg)),
            "prefix of length {cut} should be BadArg"
        );
    }
    // An advertised namelen that overruns the actual bytes.
    let mut lying = raw.clone();
    lying[9] = 0xFF;
    lying[10] = 0xFF;
    assert!(matches!(Alive2Req::decode(&lying), Err(BeamError::BadArg)));
    // An advertised extralen that overruns the actual bytes.
    let mut lying = raw.clone();
    lying[16] = 0xFF;
    lying[17] = 0xFF;
    assert!(matches!(Alive2Req::decode(&lying), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// ALIVE2_RESP / ALIVE2_X_RESP
// ---------------------------------------------------------------------------

#[test]
fn alive2_resp_legacy_ok_byte_exact_and_roundtrip() {
    let resp = Alive2Resp::Legacy { ok: true, creation: 0x1234 };
    let raw = body!(&resp);
    assert_eq!(raw, vec![b'y', 0x00, 0x12, 0x34]);
    assert_eq!(raw[0], EPMD_ALIVE2_RESP);
    assert_eq!(raw[0], 121);
    assert_eq!(Alive2Resp::decode(&raw).expect("decode"), resp);
}

#[test]
fn alive2_resp_legacy_fail() {
    let resp = Alive2Resp::Legacy { ok: false, creation: 0x0099 };
    let raw = body!(&resp);
    assert_eq!(raw, vec![b'y', 0x01, 0x00, 0x99]);
    assert_eq!(Alive2Resp::decode(&raw).expect("decode"), resp);
}

#[test]
fn alive2_resp_extended_ok_byte_exact_and_roundtrip() {
    let resp = Alive2Resp::Extended { ok: true, creation: 0x1234_5678 };
    let raw = body!(&resp);
    assert_eq!(raw, vec![b'v', 0x00, 0x12, 0x34, 0x56, 0x78]);
    assert_eq!(raw[0], EPMD_ALIVE2_X_RESP);
    assert_eq!(raw[0], 118);
    assert_eq!(Alive2Resp::decode(&raw).expect("decode"), resp);

    let resp = Alive2Resp::Extended { ok: false, creation: 0x0000_0099 };
    assert_eq!(Alive2Resp::decode(&body!(&resp)).expect("decode"), resp);
}

#[test]
fn alive2_resp_rejects_wrong_or_short_tag() {
    // 'z' is not an ALIVE2 response tag.
    assert!(matches!(Alive2Resp::decode(&[b'z', 0x00, 0x12, 0x34]), Err(BeamError::BadArg)));
    assert!(matches!(Alive2Resp::decode(&[]), Err(BeamError::BadArg)));
    // Tag present but the result byte is missing.
    assert!(matches!(Alive2Resp::decode(&[b'y']), Err(BeamError::BadArg)));
    // Result byte present but the creation field is missing / short.
    assert!(matches!(Alive2Resp::decode(&[b'y', 0x00]), Err(BeamError::BadArg)));
    assert!(matches!(Alive2Resp::decode(&[b'y', 0x00, 0x12]), Err(BeamError::BadArg)));
    assert!(matches!(Alive2Resp::decode(&[b'v', 0x00, 0x12]), Err(BeamError::BadArg)));
    // A legacy reply with a trailing byte is rejected.
    assert!(matches!(
        Alive2Resp::decode(&[b'y', 0x00, 0x12, 0x34, 0x00]),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// PORT2_REQ / PORT2_RESP
// ---------------------------------------------------------------------------

#[test]
fn port2_req_byte_exact_and_roundtrip() {
    let req = Port2Req { name: b"gurka".to_vec() };
    let raw = body!(&req);
    assert_eq!(raw, vec![b'z', b'g', b'u', b'r', b'k', b'a']);
    assert_eq!(raw[0], EPMD_PORT2_REQ);
    assert_eq!(raw[0], 122);
    assert_eq!(Port2Req::decode(&raw).expect("decode"), req);

    // The name is the whole remainder of the body.
    let empty = Port2Req { name: Vec::new() };
    assert_eq!(body!(&empty), vec![b'z']);
    assert_eq!(Port2Req::decode(&[b'z']).expect("decode"), empty);

    // Wrong tag / empty body.
    assert!(matches!(Port2Req::decode(&[b'x']), Err(BeamError::BadArg)));
    assert!(matches!(Port2Req::decode(&[]), Err(BeamError::BadArg)));
}

/// The `Ok` body per `epmd_srv.c`: `'w' + result(1)=0 + port(2) + ntype(1) +
/// proto(1) + high(2) + low(2) + namelen(2) + name + extralen(2) + extra`.
#[test]
fn port2_resp_ok_byte_exact_and_roundtrip() {
    let resp = Port2Resp::Ok {
        port: 0x1F90,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: 6,
        low_vsn: 6,
        name: b"gurka".to_vec(),
        extra: Vec::new(),
    };
    let expected: Vec<u8> = vec![
        b'w', // EPMD_PORT2_RESP (119)
        0x00, // result = 0 (ok)
        0x1F, 0x90, // port
        b'M',  // node type
        0x00, // protocol
        0x00, 0x06, // high vsn
        0x00, 0x06, // low vsn
        0x00, 0x05, // namelen
        b'g', b'u', b'r', b'k', b'a', // name
        0x00, 0x00, // extralen
    ];
    let raw = body!(&resp);
    assert_eq!(raw, expected);
    assert_eq!(raw[0], EPMD_PORT2_RESP);
    assert_eq!(raw[0], 119);
    assert_eq!(Port2Resp::decode(&raw).expect("decode"), resp);

    // A hidden node with a non-empty extra field also round-trips.
    let resp = Port2Resp::Ok {
        port: 0x1234,
        node_type: EPMD_NODE_HIDDEN,
        protocol: EPMD_PROTO_TCP,
        high_vsn: 6,
        low_vsn: 5,
        name: b"cnode".to_vec(),
        extra: b"xy".to_vec(),
    };
    assert_eq!(Port2Resp::decode(&body!(&resp)).expect("decode"), resp);
}

#[test]
fn port2_resp_error_byte_exact() {
    let resp = Port2Resp::Error;
    // The C server answers a failed lookup with exactly two bytes and
    // `wbuf[1] = 1` (`epmd_srv.c`, `case EPMD_PORT2_REQ`).
    assert_eq!(body!(&resp), vec![b'w', 0x01]);
    assert_eq!(Port2Resp::decode(&[b'w', 0x01]).expect("decode"), Port2Resp::Error);
    // Any non-zero result byte means "not registered".
    assert_eq!(Port2Resp::decode(&[b'w', 0x02]).expect("decode"), Port2Resp::Error);
}

#[test]
fn port2_resp_rejects_short_or_mismatched_body() {
    assert!(matches!(Port2Resp::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(Port2Resp::decode(&[b'z']), Err(BeamError::BadArg)));
    // Tag present but the result byte is missing.
    assert!(matches!(Port2Resp::decode(&[b'w']), Err(BeamError::BadArg)));

    let full = body!(&Port2Resp::Ok {
        port: 0x1F90,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: 6,
        low_vsn: 6,
        name: b"gurka".to_vec(),
        extra: Vec::new(),
    });
    // The `Ok` branch truncated at every point.
    for cut in 0..full.len() {
        assert!(
            matches!(Port2Resp::decode(&full[..cut]), Err(BeamError::BadArg)),
            "prefix of length {cut} should be BadArg"
        );
    }
    // An `Ok` body with a trailing byte is mismatched.
    let mut trailing = full.clone();
    trailing.push(0x00);
    assert!(matches!(Port2Resp::decode(&trailing), Err(BeamError::BadArg)));
    // An advertised namelen that overruns the body is rejected.
    let mut lying = full.clone();
    lying[10] = 0xFF;
    lying[11] = 0xFF;
    assert!(matches!(Port2Resp::decode(&lying), Err(BeamError::BadArg)));
    // An error reply with a trailing byte is rejected too.
    assert!(matches!(Port2Resp::decode(&[b'w', 0x01, 0x00]), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// NAMES
// ---------------------------------------------------------------------------

#[test]
fn names_resp_roundtrip() {
    let resp = NamesResp { epmd_port: 4369, names: vec!["a".to_string(), "bc".to_string()] };
    let raw = body!(&resp);
    // The body is the 4-byte big-endian epmd port followed by one line per node.
    assert_eq!(&raw[..4], &[0x00, 0x00, 0x11, 0x11]);
    let text = std::str::from_utf8(&raw[4..]).expect("utf8");
    assert_eq!(text, "name a at port 4369\nname bc at port 4369\n");
    assert_eq!(NamesResp::decode(&raw).expect("decode"), resp);
}

#[test]
fn names_resp_decode_handbuilt() {
    // Port 4369 then the exact server line format (`epmd_srv.c` NAMES handling:
    // `"name " + symname + " at port " + port + "\n"`).
    let mut raw = vec![0x00, 0x00, 0x11, 0x11];
    raw.extend_from_slice(b"name a at port 1000\nname bc at port 2000\n");
    let decoded = NamesResp::decode(&raw).expect("decode");
    assert_eq!(
        decoded,
        NamesResp { epmd_port: 4369, names: vec!["a".to_string(), "bc".to_string()] }
    );
}

#[test]
fn names_resp_decode_empty_node_list() {
    // An epmd with no registered nodes answers with just the 4-byte port.
    let decoded = NamesResp::decode(&[0x00, 0x00, 0x11, 0x11]).expect("decode");
    assert_eq!(decoded, NamesResp { epmd_port: 4369, names: Vec::new() });
}

#[test]
fn names_resp_rejects_malformed_body() {
    // Shorter than the 4-byte epmd port.
    assert!(matches!(NamesResp::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(NamesResp::decode(&[0x00, 0x00, 0x11]), Err(BeamError::BadArg)));
    // A non-numeric port field.
    let mut bad = vec![0x00, 0x00, 0x11, 0x11];
    bad.extend_from_slice(b"name a at port x\n");
    assert!(matches!(NamesResp::decode(&bad), Err(BeamError::BadArg)));
    // Missing the `name ` prefix.
    let mut bad = vec![0x00, 0x00, 0x11, 0x11];
    bad.extend_from_slice(b"garbage\n");
    assert!(matches!(NamesResp::decode(&bad), Err(BeamError::BadArg)));
    // Missing the trailing newline.
    let mut bad = vec![0x00, 0x00, 0x11, 0x11];
    bad.extend_from_slice(b"name a at port 1000");
    assert!(matches!(NamesResp::decode(&bad), Err(BeamError::BadArg)));
    // Missing the ` at port ` separator.
    let mut bad = vec![0x00, 0x00, 0x11, 0x11];
    bad.extend_from_slice(b"name a\n");
    assert!(matches!(NamesResp::decode(&bad), Err(BeamError::BadArg)));
    // Invalid UTF-8 in the tail.
    assert!(matches!(
        NamesResp::decode(&[0x00, 0x00, 0x11, 0x11, 0xFF, 0xFE]),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// EpmdClient
// ---------------------------------------------------------------------------

#[test]
fn epmd_client_publish_frames_request_and_decodes_reply() {
    // The *reply* is not length-framed: `epmd` writes the body straight to the
    // socket, so the tag byte is the first byte the client reads (`epmd_srv.c`
    // `reply()`; `epmd_port.c:139`).
    let reply = body!(&Alive2Resp::Legacy { ok: true, creation: 0x1234 });
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(reply));
    let mut client = EpmdClient::new(client_t);

    let got = client.publish(&gurka_req()).expect("publish");
    assert_eq!(got, Alive2Resp::Legacy { ok: true, creation: 0x1234 });

    // The *request*, by contrast, is 2-byte framed: the frame + the 18-byte
    // ALIVE2_REQ body.
    let expected = framed!(&body!(&gurka_req()));
    assert_eq!(expected.len(), 2 + 18);
    assert_eq!(probe.written(), expected);
    assert_eq!(probe.written()[0..2], [0x00, 0x12]);
    assert_eq!(probe.written()[2], b'x');
}

#[test]
fn epmd_client_publish_decodes_extended_reply() {
    // `high_vsn >= 6` makes the server answer with the 6-byte `'v'` form
    // (`epmd_srv.c:850-853`).
    let reply = body!(&Alive2Resp::Extended { ok: true, creation: 0xDEAD_BEEF });
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(reply));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(
        client.publish(&gurka_req()).expect("publish"),
        Alive2Resp::Extended { ok: true, creation: 0xDEAD_BEEF }
    );
    assert_eq!(probe.written(), framed!(&body!(&gurka_req())));
}

#[test]
fn epmd_client_port_please_frames_request_and_decodes_reply() {
    let resp = Port2Resp::Ok {
        port: 0x1F90,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: 6,
        low_vsn: 6,
        name: b"gurka".to_vec(),
        extra: Vec::new(),
    };
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(body!(&resp)));
    let mut client = EpmdClient::new(client_t);

    assert_eq!(client.port_please(b"gurka").expect("port_please"), resp);

    // The client wrote `[0x00, 0x06, 'z', 'g', 'u', 'r', 'k', 'a']`.
    assert_eq!(probe.written(), framed!(&body!(&Port2Req { name: b"gurka".to_vec() })));
    assert_eq!(probe.written(), vec![0x00, 0x06, b'z', b'g', b'u', b'r', b'k', b'a']);
}

/// `PORT2_REQ` framing: the 2-byte frame length is `1 + name.len()` (the tag plus
/// the name — `erl_epmd.erl:527` `?int16(1+length(Name))`), and the name is the
/// **remainder** of the packet with no length field of its own
/// (`erl_epmd.c:100-118`, `epmd_srv.c:894` `char *name = &buf[1];`).
#[test]
fn port2_req_frame_length_tracks_the_name_and_the_name_is_the_remainder() {
    for name in [&b""[..], b"a", b"gurka", b"genesisprobe", b"averylongnodenametoo"] {
        let body = body!(&Port2Req { name: name.to_vec() });

        // `'z' + name`, nothing else.
        assert_eq!(body[0], EPMD_PORT2_REQ);
        assert_eq!(&body[1..], name, "the name must be the packet remainder");

        // The 2-byte frame length counts the tag and the name, and the frame body
        // carries no inner length for the name.
        let framed = framed!(&body);
        let frame_len = usize::from(u16::from_be_bytes([framed[0], framed[1]]));
        assert_eq!(frame_len, 1 + name.len());
        assert_eq!(framed.len(), 2 + 1 + name.len());
    }
}

#[test]
fn epmd_client_port_please_decodes_error_reply() {
    // An unregistered name gets the minimal two-byte `'w' + result` reply.
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'w', 0x01]));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.port_please(b"nope").expect("port_please"), Port2Resp::Error);
    assert_eq!(probe.written(), vec![0x00, 0x05, b'z', b'n', b'o', b'p', b'e']);
}

#[test]
fn epmd_client_names_frames_request_and_decodes_reply() {
    let resp = NamesResp { epmd_port: 4369, names: vec!["a".to_string(), "bc".to_string()] };
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(body!(&resp)));
    let mut client = EpmdClient::new(client_t);

    assert_eq!(client.names().expect("names"), resp);

    // The NAMES request is the single byte `'n'`, 2-byte framed.
    assert_eq!(probe.written(), vec![0x00, 0x01, EPMD_NAMES_REQ]);
    assert_eq!(probe.written(), vec![0x00, 0x01, b'n']);
}

#[test]
fn epmd_client_truncated_reply_is_badarg() {
    // `'y'` promises a 2-byte creation that never arrives.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'y', 0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));

    // A successful result byte with none of the eight node-block bytes following.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'w', 0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    // A NAMES reply shorter than its 4-byte epmd port.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![0x00, 0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.names(), Err(BeamError::BadArg)));
}

#[test]
fn epmd_client_empty_input_is_badarg_not_panic() {
    // A reply that never arrives must surface as `BadArg` — never a panic and never
    // a wrong value.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));

    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.names(), Err(BeamError::BadArg)));

    // A reply cut off inside its tag/result pair is still a `BadArg`.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'y']));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));
}

#[test]
fn epmd_client_rejects_malformed_reply_body() {
    // An unknown reply tag is rejected by the codec.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'?', 0x00, 0x00, 0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));

    // A request tag where a reply tag belongs.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'z']));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    // `'w' + result` followed by a node block that is not the size its own fields
    // claim (here: a huge `namelen` with no name bytes).
    let mut truncated = vec![b'w', 0x00];
    truncated.extend_from_slice(&[0x1F, 0x90, b'M', 0x00, 0x00, 0x06, 0x00, 0x06, 0xFF, 0xFF]);
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(truncated));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    // A NAMES reply with a malformed line (no trailing newline).
    let mut malformed = 4369u32.to_be_bytes().to_vec();
    malformed.extend_from_slice(b"name genesis at port 9000");
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(malformed));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.names(), Err(BeamError::BadArg)));
}

#[test]
fn default_epmd_addr_pairs_loopback_with_the_well_known_port() {
    assert_eq!(EPMD_DEFAULT_ADDR, format!("127.0.0.1:{}", beam_dist::EPMD_PORT));
}

// ---------------------------------------------------------------------------
// LIVE `epmd` interop
//
// Everything above drives the codecs against an in-memory transport. These tests
// talk to the **real** `epmd` on this machine over TCP, which is what pins the
// request framing (2-byte length-prefixed) and the *response* framing (not
// length-prefixed at all — see `src/epmd.rs`).
//
// They **skip loudly** (a clear `eprintln!` and an early return) when no `epmd` is
// reachable, and never fake a pass.
// ---------------------------------------------------------------------------

/// A process-unique node name (`epmd` refuses to register a name twice).
///
/// The counter — rather than a clock reading — keeps the two live tests in this
/// file apart even when they run in parallel on a coarse clock.
fn live_node_name() -> String {
    let seq = LIVE_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("beamdist{}x{}", std::process::id(), seq)
}

/// Hands out the [`live_node_name`] sequence numbers.
static LIVE_NAME_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Connects to the local `epmd`, or reports why the caller should skip.
fn live_epmd_client() -> Option<EpmdClient<beam_dist::connection::TcpTransport>> {
    match EpmdClient::connect(EPMD_DEFAULT_ADDR) {
        Ok(client) => Some(client),
        Err(err) => {
            eprintln!(
                "live_epmd: SKIP — no epmd reachable at {EPMD_DEFAULT_ADDR} ({err:?}); \
                 the codec tests above still cover the wire hermetically"
            );
            None
        }
    }
}

/// `true` when `epmd` stops reporting `name`, retried until `timeout` because the
/// de-registration follows the closing of the node's `ALIVE2` socket
/// (`epmd_srv.c:641-645` `node_unreg_sock`).
fn wait_until_unregistered(name: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match lookup_port(name) {
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => return true,
            _ => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Registers a node with the real `epmd` on one connection, then has a **second**
/// connection look the registration up.
///
/// This is the end-to-end proof of the `epmd` client: `ALIVE2_REQ` →
/// `ALIVE2_X_RESP` (a v6 requester gets the 32-bit-creation `'v'` form), then
/// `PORT2_REQ` → `PORT2_RESP` on a fresh socket, and finally the de-registration
/// when the registering socket closes.
///
/// The `ALIVE2` connection is deliberately kept open for the duration: `epmd`
/// de-registers the node the moment that socket closes.
#[test]
fn live_epmd_publish_then_a_second_connection_looks_the_port_up() {
    let Some(mut publisher) = live_epmd_client() else {
        return;
    };

    // A genuinely free loopback port, bound for the duration so nothing else can
    // claim it while `epmd` holds it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("local_addr").port();
    let name = live_node_name();

    let req = Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: name.clone().into_bytes(),
        extra: Vec::new(),
    };
    let resp = match publisher.publish(&req) {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!(
                "live_epmd: SKIP — {EPMD_DEFAULT_ADDR} accepted the connection but not an \
                 ALIVE2_REQ ({err:?}); the codec tests above still cover the wire hermetically"
            );
            return;
        }
    };
    let registered = match &resp {
        Alive2Resp::Legacy { ok, .. } | Alive2Resp::Extended { ok, .. } => *ok,
    };
    assert!(registered, "epmd refused to register {name} on port {port}: {resp:?}");
    eprintln!("live_epmd: registered {name} on port {port} → {resp:?}");

    // A SECOND connection (a fresh socket) does the lookup.
    match lookup(name.as_bytes()).expect("lookup") {
        Port2Resp::Ok { port: found, node_type, protocol, name: found_name, .. } => {
            eprintln!("live_epmd: port_please({name}) → port {found}");
            assert_eq!(found, port, "the registered port must come back");
            assert_eq!(node_type, EPMD_NODE_NORMAL);
            assert_eq!(protocol, EPMD_PROTO_TCP);
            assert_eq!(found_name, name.as_bytes(), "the registered name must come back");
        }
        Port2Resp::Error => panic!("epmd did not know {name} right after registering it on {port}"),
    }
    // ... and the `lookup_port` shorthand, on a third connection.
    assert_eq!(lookup_port(name.as_bytes()).expect("lookup_port"), port);

    // An unregistered name is a typed `NotFound`, never a panic and never `0`.
    let absent = format!("{name}absent");
    match lookup_port(absent.as_bytes()) {
        Err(BeamError::Io(err)) => {
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "unregistered name: {err:?}");
        }
        other => panic!("an unregistered name must be NotFound, got {other:?}"),
    }

    // Closing the `ALIVE2` socket de-registers the node.
    drop(publisher);
    assert!(
        wait_until_unregistered(name.as_bytes(), Duration::from_secs(5)),
        "epmd still knows {name} after its ALIVE2 socket closed"
    );
    eprintln!("live_epmd: {name} de-registered once its ALIVE2 socket closed");
}

/// The address-taking lookups reach a **real** `epmd`: `lookup_at`/
/// `lookup_port_at` (aimed here at [`EPMD_DEFAULT_ADDR`], where they coincide
/// with the pinned `lookup`/`lookup_port` wrappers) resolve a freshly registered
/// node and report an unknown name as the same typed `NotFound`.
#[test]
fn live_epmd_address_taking_lookups_resolve_a_node() {
    let Some(mut publisher) = live_epmd_client() else {
        return;
    };

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("local_addr").port();
    let name = live_node_name();

    let req = Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: name.clone().into_bytes(),
        extra: Vec::new(),
    };
    let resp = match publisher.publish(&req) {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!(
                "live_epmd: SKIP — {EPMD_DEFAULT_ADDR} accepted the connection but not an \
                 ALIVE2_REQ ({err:?}); `lookup_at` is still exercised hermetically by \
                 tests/hermetic_epmd.rs"
            );
            return;
        }
    };
    match &resp {
        Alive2Resp::Legacy { ok, .. } | Alive2Resp::Extended { ok, .. } => {
            assert!(*ok, "epmd refused to register {name} on port {port}: {resp:?}")
        }
    }

    // `lookup_port_at` resolves the node through the real daemon.
    assert_eq!(
        lookup_port_at(EPMD_DEFAULT_ADDR, name.as_bytes()).expect("lookup_port_at"),
        port,
        "lookup_port_at must resolve the registered port"
    );
    // `lookup_at` returns the same registration as a raw `Port2Resp`.
    match lookup_at(EPMD_DEFAULT_ADDR, name.as_bytes()).expect("lookup_at") {
        Port2Resp::Ok { port: found, .. } => assert_eq!(found, port),
        Port2Resp::Error => panic!("epmd did not know {name} right after registering it"),
    }

    // An unregistered name is the same typed `NotFound` as the pinned form.
    let absent = format!("{name}absent");
    match lookup_port_at(EPMD_DEFAULT_ADDR, absent.as_bytes()) {
        Err(BeamError::Io(err)) => {
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "unregistered name: {err:?}");
        }
        other => panic!("an unregistered name must be NotFound, got {other:?}"),
    }

    // Closing the ALIVE2 socket de-registers the node.
    drop(publisher);
    assert!(
        wait_until_unregistered(name.as_bytes(), Duration::from_secs(5)),
        "epmd still knows {name} after its ALIVE2 socket closed"
    );
}

/// The cookie and node identity the `connect_to_node` live test authenticates with.
const LIVE_COOKIE: &[u8] = b"beam-dist-live-cookie";
/// The name the connecting side advertises in the `connect_to_node` live test.
const LIVE_CLIENT_NAME: &[u8] = b"beamdist_client@127.0.0.1";
/// The creation the accepting side advertises in the `connect_to_node` live test.
const LIVE_CREATION: u32 = 0x1234_5678;

/// [`connect_to_node`] end-to-end: it resolves the peer's listen port through the
/// real `epmd`, opens the TCP link and completes the connect/challenge handshake.
///
/// The "peer" is a genuine `beam_dist` listener ([`DistListener`]) registered with
/// `epmd`, so the whole path — `ALIVE2_REQ` registration, the *second* connection's
/// `PORT2_REQ` lookup, `TcpTransport::connect`, the handshake — runs against the
/// real `epmd` and real sockets rather than a substitute.
///
/// Skips loudly when no `epmd` is reachable (see [`live_epmd_client`]).
#[test]
fn live_connect_to_node_resolves_the_port_via_epmd_then_handshakes() {
    let Some(mut publisher) = live_epmd_client() else {
        return;
    };

    let listener = DistListener::bind("127.0.0.1:0").expect("bind a free loopback port");
    let port = listener.local_addr().expect("local_addr").port();
    let name = live_node_name();

    let req = Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: name.clone().into_bytes(),
        extra: Vec::new(),
    };
    let resp = match publisher.publish(&req) {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!(
                "live_epmd: SKIP — {EPMD_DEFAULT_ADDR} accepted the connection but not an \
                 ALIVE2_REQ ({err:?}); the codec tests above still cover the wire hermetically"
            );
            return;
        }
    };
    let registered = match &resp {
        Alive2Resp::Legacy { ok, .. } | Alive2Resp::Extended { ok, .. } => *ok,
    };
    assert!(registered, "epmd refused to register {name} on port {port}: {resp:?}");

    // The peer: a real distribution server on the port `epmd` now advertises.
    let peer_name = name.clone().into_bytes();
    let server = std::thread::spawn(move || {
        let transport = listener.accept()?;
        Connection::server(transport, &peer_name, LIVE_COOKIE, LIVE_CREATION)
    });

    // The client: `epmd` lookup → TCP connect → handshake, in one call.
    let mut client = connect_to_node("127.0.0.1", name.as_bytes(), LIVE_CLIENT_NAME, LIVE_COOKIE)
        .expect("connect_to_node");
    let mut server = server.join().expect("server thread").expect("server handshake");

    assert_eq!(client.peer_name(), name.as_bytes(), "the looked-up peer is the node we registered");
    assert_eq!(client.peer_creation(), LIVE_CREATION);
    assert_eq!(server.peer_name(), LIVE_CLIENT_NAME);
    eprintln!("live_epmd: connect_to_node({name}) completed the handshake with the listener");

    // ... and the resulting link carries control messages.
    let mut atoms = AtomTable::new();
    let mut server_heap = Heap::new(4096).expect("server heap");
    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));
    client.send_link(from, to, &atoms).expect("send_link");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_LINK");
    assert_eq!(got.dop, Dop::Link);
    assert_eq!(got.terms, vec![from, to]);

    drop(publisher);
}

// ---------------------------------------------------------------------------
// Interactive `epmd` requests: `DUMP_REQ` ('d'), `KILL_REQ` ('k'), `STOP_REQ`
// ('s')
//
// Ground truth: `erts/epmd/src/epmd.h` (tag bytes) + `epmd_cli.c` (the request
// bodies and the reply lengths the CLI reads) + `epmd_srv.c` `do_request` (the
// reply bytes) + the OTP suite `erts/epmd/test/epmd_SUITE.erl`
// (`unregister_others_name_1/2`, `no_live_killing`).
//
// `DUMP_REQ` is read-only. `KILL_REQ` (terminates the daemon) and `STOP_REQ`
// (unregisters a node) are **destructive**, so they are covered by codec
// round-trips only and are never sent to a real `epmd` by this suite.
// ---------------------------------------------------------------------------

/// The three interactive tag bytes are pinned from `epmd.h` (and the OTP suite's
/// own `-define`s: `$d`, `$k`, `$s`); `PORT_PLEASE2_REQ` is the historical
/// spelling of the existing `PORT2_REQ` (`122 = 'z'`), a documented alias rather
/// than a second tag.
#[test]
fn interactive_epmd_tag_values_are_pinned() {
    assert_eq!(EPMD_DUMP_REQ, 100);
    assert_eq!(EPMD_DUMP_REQ, b'd');
    assert_eq!(EPMD_KILL_REQ, 107);
    assert_eq!(EPMD_KILL_REQ, b'k');
    assert_eq!(EPMD_STOP_REQ, 115);
    assert_eq!(EPMD_STOP_REQ, b's');

    assert_eq!(EPMD_PORT_PLEASE2_REQ, EPMD_PORT2_REQ);
    assert_eq!(EPMD_PORT_PLEASE2_REQ, 122);
    assert_eq!(EPMD_PORT_PLEASE2_REQ, b'z');

    // ... and the already-established registration/lookup/names tags, so the
    // whole `epmd` vocabulary is pinned in one place.
    assert_eq!(EPMD_ALIVE2_REQ, b'x');
    assert_eq!(EPMD_ALIVE2_RESP, b'y');
    assert_eq!(EPMD_ALIVE2_X_RESP, b'v');
    assert_eq!(EPMD_PORT2_REQ, b'z');
    assert_eq!(EPMD_PORT2_RESP, b'w');
    assert_eq!(EPMD_NAMES_REQ, b'n');
}

// ---------------------------------------------------------------------------
// DUMP_REQ / DUMP_RESP
// ---------------------------------------------------------------------------

#[test]
fn dump_req_roundtrip_and_negatives() {
    // The CLI frames a one-byte body for `-dump` (`epmd_cli.c::epmd_call`
    // `put_int16(1,buf); buf[2] = what;`).
    assert_eq!(body!(&DumpReq), vec![b'd']);
    assert_eq!(body!(&DumpReq), vec![100]);
    assert_eq!(DumpReq::decode(&[b'd']).expect("decode"), DumpReq);

    // Wrong / short / trailing bytes are all rejected, never guessed.
    assert!(matches!(DumpReq::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(DumpReq::decode(&[b'k']), Err(BeamError::BadArg)));
    assert!(matches!(DumpReq::decode(&[b'd', 0x00]), Err(BeamError::BadArg)));
    assert!(matches!(DumpReq::decode(&[b'\0']), Err(BeamError::BadArg)));
}

#[test]
fn dump_resp_roundtrip_is_byte_exact() {
    let resp = DumpResp {
        port: 4369,
        lines: vec![
            DumpLine {
                kind: DumpNodeKind::Active,
                name: b"genesis".to_vec(),
                port: 9000,
                fd: 5,
            },
            DumpLine {
                kind: DumpNodeKind::OldUnused,
                name: b"beamrth2461".to_vec(),
                port: 41703,
                fd: 6,
            },
        ],
    };

    // The exact server bytes (`epmd_srv.c` `case EPMD_DUMP_REQ`): the big-endian
    // epmd port, then `active name     <NAME> at port N, fd = N\n` and
    // `old/unused name <NAME>, port = N, fd = N \n` — each followed by the NUL
    // `erts_snprintf` leaves in the reply buffer (`len += r + 1`). The five spaces
    // in the active prefix and the space before the old line's newline are
    // wire-frozen.
    let mut expected = 4369u32.to_be_bytes().to_vec();
    expected.extend_from_slice(b"active name     <genesis> at port 9000, fd = 5\n\x00");
    expected.extend_from_slice(b"old/unused name <beamrth2461>, port = 41703, fd = 6 \n\x00");
    assert_eq!(body!(&resp), expected);

    assert_eq!(DumpResp::decode(&expected).expect("decode"), resp);
}

#[test]
fn dump_resp_decodes_a_handbuilt_server_reply() {
    // The real reply shape observed on a live `epmd` (a registered node plus two
    // remembered names), byte for byte.
    let mut raw = 4369u32.to_be_bytes().to_vec();
    raw.extend_from_slice(b"active name     <genesis> at port 9000, fd = 5\n\x00");
    raw.extend_from_slice(b"old/unused name <beamrtn433>, port = 41703, fd = 6 \n\x00");
    raw.extend_from_slice(b"old/unused name <beamrtp433>, port = 41961, fd = 9 \n\x00");

    let dump = DumpResp::decode(&raw).expect("decode");
    assert_eq!(dump.port, 4369);
    assert_eq!(dump.lines.len(), 3);
    assert_eq!(
        dump.lines[0],
        DumpLine { kind: DumpNodeKind::Active, name: b"genesis".to_vec(), port: 9000, fd: 5 }
    );
    assert_eq!(
        dump.lines[1],
        DumpLine {
            kind: DumpNodeKind::OldUnused,
            name: b"beamrtn433".to_vec(),
            port: 41703,
            fd: 6,
        }
    );
    assert!(dump.lines[0].is_active());
    assert!(!dump.lines[1].is_active());
    assert_eq!(dump.active().count(), 1);
    assert_eq!(dump.active().next().map(|l| l.name.clone()), Some(b"genesis".to_vec()));
}

#[test]
fn dump_resp_decodes_an_empty_table_and_tolerates_a_missing_nul() {
    // No nodes at all: just the epmd port.
    assert_eq!(
        DumpResp::decode(&4369u32.to_be_bytes()).expect("decode"),
        DumpResp { port: 4369, lines: Vec::new() }
    );

    // The NUL after a line is a reply-buffer artefact, not a field: a body that
    // omits it still parses.
    let mut raw = 4369u32.to_be_bytes().to_vec();
    raw.extend_from_slice(b"active name     <genesis> at port 9000, fd = 5\n");
    let dump = DumpResp::decode(&raw).expect("decode without the NUL");
    assert_eq!(dump.lines.len(), 1);
    assert_eq!(dump.lines[0].name, b"genesis");

    // A UTF-8 node name (epmd enforces UTF-8 names) round-trips.
    let mut raw = 4369u32.to_be_bytes().to_vec();
    raw.extend_from_slice("active name     <\u{1f608}> at port 4711, fd = 7\n".as_bytes());
    let dump = DumpResp::decode(&raw).expect("decode a unicode name");
    assert_eq!(dump.lines[0].name, "\u{1f608}".as_bytes());
}

#[test]
fn dump_resp_rejects_malformed_bodies() {
    // Shorter than the 4-byte epmd port.
    assert!(matches!(DumpResp::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(DumpResp::decode(&[0x00, 0x00, 0x11]), Err(BeamError::BadArg)));

    let port = 4369u32.to_be_bytes();
    let with_tail = |tail: &[u8]| {
        let mut raw = port.to_vec();
        raw.extend_from_slice(tail);
        raw
    };

    // A line without its terminating newline.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis> at port 9000, fd = 5")),
        Err(BeamError::BadArg)
    ));
    // An unrecognised line shape.
    assert!(matches!(DumpResp::decode(&with_tail(b"bogus line\n")), Err(BeamError::BadArg)));
    // A bare newline (empty line).
    assert!(matches!(DumpResp::decode(&with_tail(b"\n")), Err(BeamError::BadArg)));
    // A NUL with no line at all.
    assert!(matches!(DumpResp::decode(&with_tail(b"\x00")), Err(BeamError::BadArg)));
    // Active line: missing the `>` separator.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis at port 9000, fd = 5\n")),
        Err(BeamError::BadArg)
    ));
    // Active line: missing the ` at port ` separator.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis> port 9000, fd = 5\n")),
        Err(BeamError::BadArg)
    ));
    // Active line: missing the `, fd = ` separator.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis> at port 9000 fd = 5\n")),
        Err(BeamError::BadArg)
    ));
    // Active line: a non-numeric port.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis> at port x, fd = 5\n")),
        Err(BeamError::BadArg)
    ));
    // Active line: an empty fd.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis> at port 9000, fd = \n")),
        Err(BeamError::BadArg)
    ));
    // A port that overflows a u16.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name     <genesis> at port 70000, fd = 5\n")),
        Err(BeamError::BadArg)
    ));
    // Old/unused line: missing the `, port = ` separator.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"old/unused name <genesis> at port 9000, fd = 5 \n")),
        Err(BeamError::BadArg)
    ));
    // Old/unused line: a non-numeric fd.
    assert!(matches!(
        DumpResp::decode(&with_tail(b"old/unused name <genesis>, port = 9000, fd = z \n")),
        Err(BeamError::BadArg)
    ));
    // Invalid UTF-8 on a line (epmd names are valid UTF-8).
    assert!(matches!(DumpResp::decode(&with_tail(b"active name     <\xff\xfe> at port 1, fd = 1\n")), Err(BeamError::BadArg)));
    // The `active name` prefix must match exactly (`old/unused` and `active`
    // differ, and a shorter prefix is not the five-space server shape).
    assert!(matches!(
        DumpResp::decode(&with_tail(b"active name <genesis> at port 1, fd = 1\n")),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// KILL_REQ / KILL_RESP
// ---------------------------------------------------------------------------

#[test]
fn kill_req_roundtrip_and_negatives() {
    // `epmd_cli.c::kill_epmd` frames a one-byte body.
    assert_eq!(body!(&KillReq), vec![b'k']);
    assert_eq!(body!(&KillReq), vec![107]);
    assert_eq!(KillReq::decode(&[b'k']).expect("decode"), KillReq);

    assert!(matches!(KillReq::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(KillReq::decode(&[b'd']), Err(BeamError::BadArg)));
    assert!(matches!(KillReq::decode(&[b'k', 0x00]), Err(BeamError::BadArg)));
}

#[test]
fn kill_resp_roundtrip_and_negatives() {
    // The literal two bytes (`epmd_srv.c` `case EPMD_KILL_REQ`; the CLI reads
    // exactly two, `epmd_cli.c::kill_epmd`).
    assert_eq!(body!(&KillResp::Ok), b"OK".to_vec());
    assert_eq!(body!(&KillResp::Refused), b"NO".to_vec());
    assert_eq!(KillResp::decode(b"OK").expect("decode"), KillResp::Ok);
    assert_eq!(KillResp::decode(b"NO").expect("decode"), KillResp::Refused);
    assert!(KillResp::Ok.is_ok());
    assert!(!KillResp::Refused.is_ok());

    assert!(matches!(KillResp::decode(b""), Err(BeamError::BadArg)));
    assert!(matches!(KillResp::decode(b"O"), Err(BeamError::BadArg)));
    assert!(matches!(KillResp::decode(b"OK!"), Err(BeamError::BadArg)));
    assert!(matches!(KillResp::decode(b"NO!"), Err(BeamError::BadArg)));
    // Case-sensitive: the server writes uppercase literals only.
    assert!(matches!(KillResp::decode(b"ok"), Err(BeamError::BadArg)));
    assert!(matches!(KillResp::decode(b"yes"), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// STOP_REQ / STOP_RESP
// ---------------------------------------------------------------------------

#[test]
fn stop_req_roundtrip_and_negatives() {
    // `epmd_cli.c::stop_cli` frames `strlen(name) + 1` bytes: the tag plus the
    // name, which is the packet remainder with no length field of its own.
    let req = StopReq { name: b"gurka".to_vec() };
    let raw = body!(&req);
    assert_eq!(raw[0], EPMD_STOP_REQ);
    assert_eq!(raw[0], 115);
    assert_eq!(&raw[1..], b"gurka");
    assert_eq!(raw, vec![b's', b'g', b'u', b'r', b'k', b'a']);
    assert_eq!(StopReq::decode(&raw).expect("decode"), req);

    // A name may contain any bytes (`epmd` handles the UTF-8/255-char rules).
    let req = StopReq { name: b"\x00\xff".to_vec() };
    assert_eq!(StopReq::decode(&body!(&req)).expect("decode"), req);

    // A `STOP_REQ` carrying only its tag is not a legal request: the server
    // requires `bsize > 1` (`epmd_srv.c` "packet too small for request
    // EPMD_STOP_REQ"), so an empty name is rejected.
    assert!(matches!(StopReq::decode(&[b's']), Err(BeamError::BadArg)));
    assert!(matches!(StopReq::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(StopReq::decode(&[b'k', b'x']), Err(BeamError::BadArg)));
    assert!(matches!(StopReq::decode(&[b'z']), Err(BeamError::BadArg)));
}

#[test]
fn stop_resp_roundtrip_and_negatives() {
    // `epmd_srv.c` `case EPMD_STOP_REQ` answers `"STOPPED"` / `"NOEXIST"` (7 bytes
    // each; the CLI reads exactly seven). Both are pinned by the OTP suite
    // (`epmd_SUITE.erl::unregister_others_name_1/2`). NOTE: the success reply is
    // `"STOPPED"`, *not* the 2-byte `"OK"` — that is `KILL_RESP`.
    assert_eq!(body!(&StopResp::Stopped), b"STOPPED".to_vec());
    assert_eq!(body!(&StopResp::NoExist), b"NOEXIST".to_vec());
    // The "declined without answering" outcome encodes to nothing, because the
    // server sends nothing.
    assert_eq!(body!(&StopResp::NoReply), Vec::<u8>::new());

    assert_eq!(StopResp::decode(b"STOPPED").expect("decode"), StopResp::Stopped);
    assert_eq!(StopResp::decode(b"NOEXIST").expect("decode"), StopResp::NoExist);
    assert_eq!(StopResp::decode(b"").expect("decode"), StopResp::NoReply);
    assert!(StopResp::Stopped.is_stopped());
    assert!(!StopResp::NoExist.is_stopped());
    assert!(!StopResp::NoReply.is_stopped());

    assert!(matches!(StopResp::decode(b"OK"), Err(BeamError::BadArg)));
    assert!(matches!(StopResp::decode(b"STOP"), Err(BeamError::BadArg)));
    assert!(matches!(StopResp::decode(b"STOPPED!"), Err(BeamError::BadArg)));
    assert!(matches!(StopResp::decode(b"NOEXIST "), Err(BeamError::BadArg)));
    assert!(matches!(StopResp::decode(b"stopped"), Err(BeamError::BadArg)));
    assert!(matches!(StopResp::decode(b"\x00"), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// EpmdClient: the three interactive exchanges over `MemoryTransport`
// ---------------------------------------------------------------------------

#[test]
fn epmd_client_dump_frames_request_and_decodes_reply() {
    let resp = DumpResp {
        port: 4369,
        lines: vec![DumpLine {
            kind: DumpNodeKind::Active,
            name: b"gurka".to_vec(),
            port: 9000,
            fd: 5,
        }],
    };
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(body!(&resp)));
    let mut client = EpmdClient::new(client_t);

    assert_eq!(client.dump().expect("dump"), resp);
    // The request is the single byte `'d'`, 2-byte framed.
    assert_eq!(probe.written(), vec![0x00, 0x01, EPMD_DUMP_REQ]);
    assert_eq!(probe.written(), vec![0x00, 0x01, b'd']);
}

#[test]
fn epmd_client_kill_frames_request_and_decodes_reply() {
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(b"NO".to_vec()));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.kill().expect("kill"), KillResp::Refused);
    assert_eq!(probe.written(), vec![0x00, 0x01, EPMD_KILL_REQ]);
    assert_eq!(probe.written(), vec![0x00, 0x01, b'k']);

    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(b"OK".to_vec()));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.kill().expect("kill"), KillResp::Ok);
    assert_eq!(probe.written(), vec![0x00, 0x01, b'k']);
}

#[test]
fn epmd_client_stop_frames_request_and_decodes_each_reply() {
    // `"STOPPED"`: the frame length is `1 + name.len()` (the tag plus "gurka").
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(b"STOPPED".to_vec()));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.stop(b"gurka").expect("stop"), StopResp::Stopped);
    assert_eq!(probe.written(), vec![0x00, 0x06, b's', b'g', b'u', b'r', b'k', b'a']);

    // `"NOEXIST"`.
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(b"NOEXIST".to_vec()));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.stop(b"nope").expect("stop"), StopResp::NoExist);
    assert_eq!(probe.written(), vec![0x00, 0x05, b's', b'n', b'o', b'p', b'e']);

    // No reply at all (the daemon declined without answering): an empty transport
    // is the `NoReply` outcome, never a hang and never a wrong verdict.
    let (client_t, probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.stop(b"gurka").expect("stop"), StopResp::NoReply);
    assert_eq!(probe.written(), vec![0x00, 0x06, b's', b'g', b'u', b'r', b'k', b'a']);}

#[test]
fn epmd_client_interactive_truncated_replies_are_badarg() {
    // KILL: only one of the two literal bytes arrives.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![b'O']));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.kill(), Err(BeamError::BadArg)));

    // STOP: `"STOP"` — the first byte arrives, the remaining six do not. (A
    // reply cut off *after* the first byte is a truncated 7-byte reply, not the
    // "no reply" outcome.)
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(b"STOP".to_vec()));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.stop(b"gurka"), Err(BeamError::BadArg)));

    // DUMP: no port at all.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.dump(), Err(BeamError::BadArg)));
}

#[test]
fn epmd_client_dump_read_is_bounded() {
    // A wedged/hostile peer that streams an endless body must not drive unbounded
    // allocation: the read stops at the ceiling with a typed `SystemLimit`.
    let flood = vec![b'x'; MAX_DUMP_RESP_BODY + 16];
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(flood));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.dump(), Err(BeamError::SystemLimit)));
}

// ---------------------------------------------------------------------------
// Typed failure classification
// ---------------------------------------------------------------------------

#[test]
fn alive2_resp_distinguishes_acceptance_from_an_alive_conflict() {
    let accepted = Alive2Resp::Extended { ok: true, creation: 0xDEAD_BEEF };
    assert!(accepted.is_accepted());
    assert!(!accepted.is_rejected());
    assert!(!accepted.is_alive_conflict());
    assert_eq!(accepted.creation(), 0xDEAD_BEEF);

    // A non-zero result byte (`epmd_srv.c` `wbuf[1] = node ? 0 : 1`) is a refusal:
    // in practice the name is already registered ("node name already occupied").
    let conflict = Alive2Resp::Legacy { ok: false, creation: 0 };
    assert!(conflict.is_rejected());
    assert!(conflict.is_alive_conflict());
    assert!(!conflict.is_accepted());
    assert_eq!(conflict.creation(), 0);

    // The same, decoded from the wire in both creation widths.
    let decoded = Alive2Resp::decode(&[b'y', 0x01, 0x00, 0x00]).expect("decode");
    assert!(decoded.is_alive_conflict());
    assert_eq!(decoded.creation(), 0);
    let decoded = Alive2Resp::decode(&[b'y', 0x00, 0x12, 0x34]).expect("decode");
    assert!(decoded.is_accepted());
    assert_eq!(decoded.creation(), 0x1234);
    let decoded = Alive2Resp::decode(&[b'v', 0x00, 0xDE, 0xAD, 0xBE, 0xEF]).expect("decode");
    assert!(decoded.is_accepted());
    assert_eq!(decoded.creation(), 0xDEAD_BEEF);
}

#[test]
fn port2_resp_distinguishes_registered_from_not_registered() {
    let found = Port2Resp::Ok {
        port: 9000,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: 6,
        low_vsn: 6,
        name: b"gurka".to_vec(),
        extra: Vec::new(),
    };
    assert!(found.is_registered());
    assert!(!found.is_not_registered());

    // The `'w' + result(1)` "not registered" reply (result byte != 0).
    for body in [&[b'w', 0x01][..], &[b'w', 0x02][..]] {
        let resp = Port2Resp::decode(body).expect("decode");
        assert!(resp.is_not_registered());
        assert!(!resp.is_registered());
        assert_eq!(resp, Port2Resp::Error);
    }
}

// ---------------------------------------------------------------------------
// LIVE `epmd` DUMP (read-only; `KILL`/`STOP` are never sent to a real daemon)
// ---------------------------------------------------------------------------

/// `DUMP_REQ` is the only interactive code that is **read-only**, so it is the
/// only one exercised against the machine's real `epmd` (which requires only a
/// loopback peer). `KILL_REQ`/`STOP_REQ` would shut the daemon down or
/// de-register a node and are therefore codec-covered only.
///
/// The test registers its own uniquely named node first, so the assertion does
/// not depend on whatever else happens to be registered on the host. It skips
/// loudly when no `epmd` is reachable.
#[test]
fn live_epmd_dump_lists_a_node_we_just_registered() {
    // `epmd` closes the connection right after answering a DUMP (only ALIVE2 sets
    // `keep`), so the body ends at EOF; a short read timeout still bounds the test
    // against a peer that holds the socket open.
    let mut transport = match TcpTransport::connect(EPMD_DEFAULT_ADDR) {        Ok(transport) => transport,
        Err(err) => {
            eprintln!(
                "live_epmd: SKIP — no epmd reachable at {EPMD_DEFAULT_ADDR} ({err:?}); \
                 the codec tests above still cover the wire hermetically"
            );
            return;
        }
    };
    transport.set_read_timeout(Some(Duration::from_secs(2))).expect("set_read_timeout");

    let Some(mut publisher) = live_epmd_client() else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("local_addr").port();
    let name = live_node_name();

    let req = Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: name.clone().into_bytes(),
        extra: Vec::new(),
    };
    let resp = match publisher.publish(&req) {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!(
                "live_epmd: SKIP — {EPMD_DEFAULT_ADDR} accepted the connection but not an \
                 ALIVE2_REQ ({err:?}); the codec tests above still cover the wire hermetically"
            );
            return;
        }
    };
    // The typed acceptance accessor replaces the previous `ok`-field match.
    assert!(resp.is_accepted(), "epmd refused to register {name} on port {port}: {resp:?}");

    // The dump is read over a *second* connection, read-only.
    let mut client = EpmdClient::new(transport);
    let dump = match client.dump() {
        Ok(dump) => dump,
        Err(err) => {
            eprintln!("live_epmd: SKIP — the real epmd did not answer DUMP_REQ ({err:?})");
            return;
        }
    };

    // The dump starts with epmd's own port.
    assert_eq!(dump.port, u32::from(beam_dist::EPMD_PORT), "the dump reports epmd's own port");
    eprintln!(
        "live_epmd: dump parsed {} node lines (epmd port {}), first = {:?}",
        dump.lines.len(),
        dump.port,
        dump.lines.first()
    );

    // Every line decoded into a typed record, and our freshly registered node is
    // listed as active on the port we registered.
    let ours = dump
        .lines
        .iter()
        .find(|line| line.name == name.as_bytes())
        .unwrap_or_else(|| {
            panic!(
                "the dump must list {name}; it reported: {:?}",
                dump.lines
                    .iter()
                    .map(|l| String::from_utf8_lossy(&l.name).into_owned())
                    .collect::<Vec<_>>()
            )
        });
    assert!(ours.is_active(), "{name} must be an active node: {ours:?}");
    assert_eq!(ours.port, port, "the dump must report the registered port");
    assert!(ours.fd > 0, "an active node has a live descriptor: {ours:?}");
    assert!(dump.active().any(|line| line.name == name.as_bytes()));
    assert!(
        dump.active().count() <= dump.lines.len(),
        "the active view is a subset of all lines"
    );

    // Closing the ALIVE2 socket de-registers the node again, so the test leaves
    // no trace in the host's port mapper.
    drop(publisher);
    assert!(
        wait_until_unregistered(name.as_bytes(), Duration::from_secs(5)),
        "epmd still knows {name} after its ALIVE2 socket closed"
    );
    eprintln!("live_epmd: {name} de-registered after the DUMP test");
}