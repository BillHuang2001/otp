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

use beam_common::BeamError;
use beam_dist::epmd::{
    Alive2Req, Alive2Resp, EpmdClient, NamesResp, Port2Req, Port2Resp, EPMD_ALIVE2_REQ,
    EPMD_ALIVE2_RESP, EPMD_ALIVE2_X_RESP, EPMD_NAMES_REQ, EPMD_NODE_HIDDEN, EPMD_NODE_NORMAL,
    EPMD_PORT2_REQ, EPMD_PORT2_RESP, EPMD_PROTO_TCP,
};
use beam_dist::framing::{write_packet, PacketHeaderSize};
use beam_dist::transport::{MemoryTransport, Read, Write};

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
    let reply = body!(&Alive2Resp::Legacy { ok: true, creation: 0x1234 });
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&reply)));
    let mut client = EpmdClient::new(client_t);

    let got = client.publish(&gurka_req()).expect("publish");
    assert_eq!(got, Alive2Resp::Legacy { ok: true, creation: 0x1234 });

    // The client wrote the 2-byte frame + the ALIVE2_REQ body (18 bytes).
    let expected = framed!(&body!(&gurka_req()));
    assert_eq!(expected.len(), 2 + 18);
    assert_eq!(probe.written(), expected);
    assert_eq!(probe.written()[0..2], [0x00, 0x12]);
    assert_eq!(probe.written()[2], b'x');
}

#[test]
fn epmd_client_publish_decodes_extended_reply() {
    let reply = body!(&Alive2Resp::Extended { ok: true, creation: 0xDEAD_BEEF });
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&reply)));
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
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&body!(&resp))));
    let mut client = EpmdClient::new(client_t);

    assert_eq!(client.port_please(b"gurka").expect("port_please"), resp);

    // The client wrote `[0x00, 0x06, 'z', 'g', 'u', 'r', 'k', 'a']`.
    assert_eq!(probe.written(), framed!(&body!(&Port2Req { name: b"gurka".to_vec() })));
    assert_eq!(probe.written(), vec![0x00, 0x06, b'z', b'g', b'u', b'r', b'k', b'a']);
}

#[test]
fn epmd_client_port_please_decodes_error_reply() {
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&[b'w', 0x01])));
    let mut client = EpmdClient::new(client_t);
    assert_eq!(client.port_please(b"nope").expect("port_please"), Port2Resp::Error);
    assert_eq!(probe.written(), vec![0x00, 0x05, b'z', b'n', b'o', b'p', b'e']);
}

#[test]
fn epmd_client_names_frames_request_and_decodes_reply() {
    let resp = NamesResp { epmd_port: 4369, names: vec!["a".to_string(), "bc".to_string()] };
    let (client_t, probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&body!(&resp))));
    let mut client = EpmdClient::new(client_t);

    assert_eq!(client.names().expect("names"), resp);

    // The NAMES request is the single byte `'n'`, 2-byte framed.
    assert_eq!(probe.written(), vec![0x00, 0x01, EPMD_NAMES_REQ]);
    assert_eq!(probe.written(), vec![0x00, 0x01, b'n']);
}

#[test]
fn epmd_client_truncated_framed_reply_is_badarg() {
    // The frame advertises 4 bytes but only 2 follow.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![0x00, 0x04, b'y', 0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));

    // The frame advertises a body but none follows.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![0x00, 0x02]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    // A complete frame whose body is empty (a tick) is not a valid reply.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![0x00, 0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.names(), Err(BeamError::BadArg)));
}

#[test]
fn epmd_client_empty_input_is_badarg_not_panic() {
    // A short read on the length prefix must surface as `BadArg` — never a panic.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));

    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    let (client_t, _probe) = SharedTransport::new(MemoryTransport::new());
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.names(), Err(BeamError::BadArg)));

    // A one-byte input is still a short length prefix.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(vec![0x00]));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));
}

#[test]
fn epmd_client_rejects_malformed_reply_body() {
    // A well-framed body with an unknown tag is rejected by the codec.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&[b'?', 0x00])));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.publish(&gurka_req()), Err(BeamError::BadArg)));

    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&[b'z'])));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.port_please(b"gurka"), Err(BeamError::BadArg)));

    // A NAMES reply shorter than its 4-byte epmd port.
    let (client_t, _probe) = SharedTransport::new(MemoryTransport::with_input(framed!(&[0x00, 0x11])));
    let mut client = EpmdClient::new(client_t);
    assert!(matches!(client.names(), Err(BeamError::BadArg)));
}
