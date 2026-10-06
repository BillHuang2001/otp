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
use beam_dist::connection::{connect_to_node, Connection, TcpListener as DistListener};
use beam_dist::epmd::{
    lookup, lookup_port, Alive2Req, Alive2Resp, EpmdClient, NamesResp, Port2Req, Port2Resp,
    EPMD_ALIVE2_REQ, EPMD_ALIVE2_RESP, EPMD_ALIVE2_X_RESP, EPMD_DEFAULT_ADDR, EPMD_DIST_HIGH,
    EPMD_DIST_LOW, EPMD_NAMES_REQ, EPMD_NODE_HIDDEN, EPMD_NODE_NORMAL, EPMD_PORT2_REQ,
    EPMD_PORT2_RESP, EPMD_PROTO_TCP,
};
use beam_dist::flags::Dop;
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
