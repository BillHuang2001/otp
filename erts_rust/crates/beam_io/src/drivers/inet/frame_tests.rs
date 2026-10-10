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

//! Integration tests for the [`tcp_inet` driver](super)'s **packet framing** — the `{packet, N}`
//! decode implemented in [`super::frame`] and wired into the `recv` verb and
//! [`super::TcpInetDriver::read_available`], plus its **write-side counterpart**: the length
//! header the `send` verb prepends under `{packet, 1|2|4}` (the C's `tcp_sendv`/`tcp_send`).
//!
//! Split from the driver's own `tests.rs` suite (which is a sibling, concurrently edited file)
//! and kept separate because it drives the framing path over **real loopback sockets** and
//! cross-checks the delivered payloads against the system `gen_tcp` oracle.
//!
//! The sockets follow the same shape `tests.rs` uses: the driver adopts a `dup`-free copy of an
//! accepted descriptor (ownership is moved with [`IntoRawFd`]), and a raw `std::net::TcpStream`
//! plays the peer so the bytes on the wire are exactly the test's own.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::IntoRawFd;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use beam_common::Result;
use beam_process::PortId;

use crate::poller::Poller;
use crate::port::PortRegistry;

use super::packet::PacketType;
use super::{
    ctl_ok, TcpInetDriver, INET_LOPT_LINE_DELIM, INET_LOPT_PACKET,
};

/// How long a helper waits for the kernel or the peer before failing the test.
const WAIT: Duration = Duration::from_secs(5);

/// Wall-clock budget for the `erl` oracle before the affected check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

// -----------------------------------------------------------------------
// Sockets and control plumbing
// -----------------------------------------------------------------------

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// A loopback listener the test owns.
fn loopback_listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("a loopback listener")
}

/// A connected `tcp_inet` port whose peer is a raw `std::net::TcpStream` the test writes to.
///
/// The accepted descriptor's ownership is **moved** into the driver (no `dup`, so no `unsafe`),
/// and the returned client is the other end of the connection.
fn connected_port(poller: &mut Poller) -> (PortRegistry, PortId, TcpStream) {
    let listener = loopback_listener();
    let address = listener.local_addr().expect("an address");
    let client = TcpStream::connect(address).expect("the client connects");
    let (server, _peer) = listener.accept().expect("accepts the connection");
    let driver =
        TcpInetDriver::from_socket(server.into_raw_fd()).expect("the driver adopts the socket");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(poller, Box::new(driver))
        .expect("the port opens");
    (registry, id, client)
}

/// Runs one control verb on port `id`, panicking on the Rust error kind.
fn control(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    command: &str,
    arg: &[u8],
) -> Vec<u8> {
    registry
        .get_mut(id)
        .expect("the port is live")
        .control(poller, command, arg)
        .unwrap_or_else(|err| panic!("the `{command}` verb failed: {err:?}"))
}

/// Runs one control verb returning its typed outcome (for the cases a test expects to fail).
fn try_control(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    command: &str,
    arg: &[u8],
) -> Result<Vec<u8>> {
    registry
        .get_mut(id)
        .expect("the port is live")
        .control(poller, command, arg)
}

/// The five-byte `[opt, val_be32]` record `setopts` takes.
fn opt_record(code: u8, value: u32) -> Vec<u8> {
    let v = value.to_be_bytes();
    vec![code, v[0], v[1], v[2], v[3]]
}

/// `setopts` of a `{packet, N}` code (the driver's `TCP_PB_*` value, not the Erlang number).
fn set_packet(registry: &mut PortRegistry, poller: &mut Poller, id: PortId, packet: PacketType) {
    assert_eq!(
        control(
            registry,
            poller,
            id,
            "setopts",
            &opt_record(INET_LOPT_PACKET, packet.code() as u32),
        ),
        ctl_ok()
    );
}

/// `setopts` of a `{line_delimiter, C}` byte.
fn set_line_delim(registry: &mut PortRegistry, poller: &mut Poller, id: PortId, delim: u8) {
    assert_eq!(
        control(
            registry,
            poller,
            id,
            "setopts",
            &opt_record(INET_LOPT_LINE_DELIM, delim as u32),
        ),
        ctl_ok()
    );
}

/// One `recv` turn: run the verb and drain whatever it delivered into the port input queue.
fn recv_once(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
) -> Result<Vec<u8>> {
    let reply = registry
        .get_mut(id)
        .expect("the port is live")
        .control(poller, "recv", b"")?;
    assert_eq!(reply, ctl_ok(), "a framing `recv` replies `[1]`");
    Ok(registry.get_mut(id).expect("the port is live").drain_input())
}

/// Drives `recv` until a decoded frame's payload lands in the port input queue.
fn recv_frame(registry: &mut PortRegistry, poller: &mut Poller, id: PortId) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    loop {
        let got = recv_once(registry, poller, id).expect("the recv verb");
        if !got.is_empty() {
            return got;
        }
        assert!(Instant::now() < deadline, "no frame was ever delivered");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Drives `recv` (raw mode) until at least `want` bytes have been delivered.
fn recv_at_least(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.len() < want {
        assert!(Instant::now() < deadline, "no bytes were ever delivered");
        got.extend(recv_once(registry, poller, id).expect("the recv verb"));
        thread::sleep(Duration::from_millis(10));
    }
    got
}

// -----------------------------------------------------------------------
// The length-prefixed types
// -----------------------------------------------------------------------

/// `{packet, 4}`: the four-byte big-endian length header is stripped and the body delivered.
#[test]
fn packet4_delivers_the_payload_without_the_length_header() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Len4);

    client
        .write_all(&[0, 0, 0, 4, b'a', b'b', b'c', b'd'])
        .expect("the peer writes one frame");
    assert_eq!(
        recv_frame(&mut registry, &mut poller, id),
        b"abcd",
        "the 4-byte header is stripped and the body delivered"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

/// `{packet, 2}`: a two-byte big-endian length header.
#[test]
fn packet2_strips_its_two_byte_header() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Len2);

    client
        .write_all(&[0, 3, b'h', b'i', b'j'])
        .expect("the peer writes one frame");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"hij");
    registry.close(&mut poller, id).expect("closes the port");
}

/// `{packet, 1}`: a single-byte length header.
#[test]
fn packet1_strips_its_single_byte_header() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Len1);

    client
        .write_all(&[3, b'x', b'y', b'z'])
        .expect("the peer writes one frame");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"xyz");
    registry.close(&mut poller, id).expect("closes the port");
}

/// A partial header delivers nothing, is **retained**, and the frame is completed by the rest.
///
/// Three of the four `{packet, 4}` header bytes arrive first. If the driver dropped them the
/// second write (`4, 'a', 'b', 'c', 'd'`) would be read as a header (`0x04616263`) far larger
/// than the bytes on hand, and `recv` would keep answering the bare `[1]` forever — so the
/// delivered `abcd` proves the partial header was combined with the remainder, not discarded.
#[test]
fn a_partial_header_is_retained_and_delivers_nothing_until_the_frame_is_complete() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Len4);

    client
        .write_all(&[0, 0, 0])
        .expect("the peer writes a partial header");
    // However often `recv` runs, a header it cannot size yet delivers nothing.
    for _ in 0..10 {
        assert_eq!(
            recv_once(&mut registry, &mut poller, id).expect("the recv verb"),
            Vec::<u8>::new(),
            "a partial header must not deliver a frame"
        );
        thread::sleep(Duration::from_millis(10));
    }

    client
        .write_all(&[4, b'a', b'b', b'c', b'd'])
        .expect("the peer writes the rest of the frame");
    assert_eq!(
        recv_frame(&mut registry, &mut poller, id),
        b"abcd",
        "the retained header bytes are combined with the body"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

/// Exactly **one** frame is delivered per `recv`, even when two arrive together.
#[test]
fn one_frame_is_delivered_per_recv() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Len4);

    client
        .write_all(&[
            0, 0, 0, 4, b'a', b'b', b'c', b'd', // the first frame
            0, 0, 0, 3, b'x', b'y', b'z', // the second, back to back
        ])
        .expect("the peer writes two frames");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"abcd");
    assert_eq!(
        recv_frame(&mut registry, &mut poller, id),
        b"xyz",
        "the second frame is delivered by the second recv, not merged into the first"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

// -----------------------------------------------------------------------
// Line mode
// -----------------------------------------------------------------------

/// `{packet, line}` delivers the line **including** its delimiter, exactly as the oracle does
/// (the C's `packet_get_body` returns a line "as is").
#[test]
fn line_delivers_the_line_including_its_delimiter() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::LineLf);

    client
        .write_all(b"abc\ndef\n")
        .expect("the peer writes two lines");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"abc\n");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"def\n");
    registry.close(&mut poller, id).expect("closes the port");
}

/// A custom `{line_delimiter, C}` frames at that byte instead of `\n`.
#[test]
fn a_custom_line_delimiter_frames_at_that_byte() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::LineLf);
    set_line_delim(&mut registry, &mut poller, id, b'|');

    client
        .write_all(b"hello|world|")
        .expect("the peer writes two delimited lines");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"hello|");
    assert_eq!(recv_frame(&mut registry, &mut poller, id), b"world|");
    registry.close(&mut poller, id).expect("closes the port");
}

// -----------------------------------------------------------------------
// Raw mode is unchanged, and the unsupported types fail typed
// -----------------------------------------------------------------------

/// `{packet, raw}` is untouched: the whole byte stream reaches the port input queue unframed.
#[test]
fn packet_raw_still_delivers_the_whole_stream() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    // Explicit `{packet, raw}` — the same default, but it pins the raw path.
    set_packet(&mut registry, &mut poller, id, PacketType::Raw);

    let stream = b"\x00\x00\x00\x04abcd";
    client
        .write_all(stream)
        .expect("the peer writes a byte stream");
    assert_eq!(
        recv_at_least(&mut registry, &mut poller, id, stream.len()),
        stream,
        "raw mode moves every byte through, header and all"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

/// A `{packet, …}` change while a partial frame sits in the driver's buffer loses nothing: the
/// retained bytes are flushed verbatim once raw mode takes over (see
/// [`super::TcpInetDriver::pending_input`]).
///
/// `{packet, 1}` first announces a three-byte body but only one byte is written, so the frame is
/// incomplete and nothing is delivered; switching to `{packet, raw}` and writing the rest must
/// then yield the whole stream in order — the retained `{1, 'a'}` prefix included, never dropped.
#[test]
fn switching_to_raw_flushes_the_retained_partial_frame_verbatim() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Len1);

    client
        .write_all(&[3, b'a'])
        .expect("the peer writes a partial frame");
    // Read the partial frame into the framing buffer (whichever recv turn lands it there, a
    // framing decode of an incomplete frame delivers nothing).
    for _ in 0..5 {
        assert_eq!(
            recv_once(&mut registry, &mut poller, id).expect("the recv verb"),
            Vec::<u8>::new(),
            "an incomplete frame delivers nothing"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // Raw mode is authoritative for the already-queued bytes.
    set_packet(&mut registry, &mut poller, id, PacketType::Raw);
    client
        .write_all(b"c")
        .expect("the peer writes the rest of the stream");
    assert_eq!(
        recv_at_least(&mut registry, &mut poller, id, 3),
        vec![3, b'a', b'c'],
        "the bytes buffered while framing must be delivered verbatim after the switch"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

/// A `{packet, http}` request is refused with a typed error and never a wrong frame.
#[test]
fn an_unsupported_packet_type_is_a_typed_error_never_a_wrong_frame() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Http);

    client
        .write_all(b"GET / HTTP/1.0\r\n\r\n")
        .expect("the peer writes a request");
    let err = try_control(&mut registry, &mut poller, id, "recv", b"")
        .expect_err("the HTTP family is not decoded at the driver layer");
    assert_eq!(err.as_atom(), Some("notsup"), "got {err:?}");
    assert_eq!(
        registry.get_mut(id).expect("the port is live").drain_input(),
        Vec::<u8>::new(),
        "an unsupported type delivers nothing at all"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

// -----------------------------------------------------------------------
// The write path frames under {packet, 1|2|4} too
// -----------------------------------------------------------------------

/// Sends `payload` through the port's `send` verb and returns the exact `wire` bytes that reach
/// the peer.
///
/// The peer reads precisely `wire` bytes: a missing header byte (the defect this covers) leaves
/// `read_exact` short and fails the test, and an extra one is visible in the returned bytes —
/// neither can be absorbed by the stream.
fn send_and_capture(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    client: &mut TcpStream,
    wire: usize,
    payload: &[u8],
) -> Vec<u8> {
    client
        .set_read_timeout(Some(WAIT))
        .expect("a read timeout");
    assert_eq!(
        control(registry, poller, id, "send", payload),
        ctl_ok(),
        "the `send` verb"
    );
    let mut got = vec![0u8; wire];
    client
        .read_exact(&mut got)
        .expect("the peer reads exactly the announced bytes");
    got
}

/// `{packet, 4}`, `{packet, 2}` and `{packet, 1}`: the `send` verb prepends the same big-endian
/// length header the C's `tcp_sendv` does (`inet_drv.c:13745`-`:13769`), so the peer's
/// `packet_get_length` reads the message back as one frame.
#[test]
fn a_framed_send_prepends_the_length_header() {
    let payload: &[u8] = b"abc";
    for (packet, header) in [
        (PacketType::Len1, vec![3u8]),
        (PacketType::Len2, vec![0, 3]),
        (PacketType::Len4, vec![0, 0, 0, 3]),
    ] {
        let mut poller = poller();
        let (mut registry, id, mut client) = connected_port(&mut poller);
        set_packet(&mut registry, &mut poller, id, packet);

        let mut expected = header;
        expected.extend_from_slice(payload);
        assert_eq!(
            send_and_capture(
                &mut registry,
                &mut poller,
                id,
                &mut client,
                expected.len(),
                payload
            ),
            expected,
            "{packet:?}: the wire bytes must be the header followed by the payload"
        );
        registry.close(&mut poller, id).expect("closes the port");
    }
}

/// An **empty** send under a framing code still writes the bare header: the C's shortcut
/// (`if ((h_len == 0) && (len == 0)) return 0`, `inet_drv.c:13907`) fires only when there is no
/// header either — the oracle writes `<<0,0,0,0>>` for `{packet,4}` and `<<0>>` for `{packet,1}`.
#[test]
fn an_empty_send_under_a_framing_code_writes_the_bare_header() {
    for (packet, header) in [
        (PacketType::Len1, vec![0u8]),
        (PacketType::Len4, vec![0, 0, 0, 0]),
    ] {
        let mut poller = poller();
        let (mut registry, id, mut client) = connected_port(&mut poller);
        set_packet(&mut registry, &mut poller, id, packet);

        assert_eq!(
            send_and_capture(&mut registry, &mut poller, id, &mut client, header.len(), b""),
            header,
            "{packet:?}: an empty payload still announces a zero-length body"
        );
        registry.close(&mut poller, id).expect("closes the port");
    }
}

/// `{packet, raw}` — the default, and every code outside `1`/`2`/`4` — is **never** framed: the
/// payload goes out verbatim (the C's `default: h_len = 0`).
#[test]
fn a_raw_send_writes_the_payload_verbatim() {
    let mut poller = poller();
    let (mut registry, id, mut client) = connected_port(&mut poller);
    set_packet(&mut registry, &mut poller, id, PacketType::Raw);

    // The same bytes a `{packet, 4}` send would frame: nothing is prepended here.
    assert_eq!(
        send_and_capture(&mut registry, &mut poller, id, &mut client, 3, b"abc"),
        b"abc",
        "raw mode puts the payload on the wire untouched"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

// -----------------------------------------------------------------------
// The oracle cross-check: the payloads real `gen_tcp` produces
// -----------------------------------------------------------------------
/// The Erlang `{packet, …}` option whose framing matches `packet`.
fn erl_packet_option(packet: PacketType) -> &'static str {
    match packet {
        PacketType::Raw => "{packet,raw}",
        PacketType::Len1 => "{packet,1}",
        PacketType::Len2 => "{packet,2}",
        PacketType::Len4 => "{packet,4}",
        PacketType::LineLf => "{packet,line}",
        other => panic!("no Erlang {{packet, …}} option for {other:?}"),
    }
}

/// The Erlang side of the cross-check: `gen_tcp` ports with the **same** framing options, each
/// fed the same wire bytes by a raw client, printing each `gen_tcp:recv` payload as a
/// comma-separated byte list, `|`-separated between cases.
fn oracle_expression(cases: &[(Vec<u8>, PacketType)]) -> String {
    let calls: Vec<String> = cases
        .iter()
        .map(|(wire, packet)| {
            let bytes: Vec<String> = wire.iter().map(u8::to_string).collect();
            format!("F({}, <<{}>>)", erl_packet_option(*packet), bytes.join(","))
        })
        .collect();
    format!(
        "F = fun(Pkt, Wire) -> \
         {{ok,L}} = gen_tcp:listen(0, [binary,{{active,false}},{{reuseaddr,true}},Pkt]), \
         {{ok,P}} = inet:port(L), \
         spawn(fun() -> {{ok,C}} = gen_tcp:connect({{127,0,0,1}},P,[binary,{{active,false}}]), \
           ok = gen_tcp:send(C, Wire), timer:sleep(150), gen_tcp:close(C) end), \
         {{ok,S}} = gen_tcp:accept(L, 2000), \
         {{ok,B}} = gen_tcp:recv(S,0,2000), gen_tcp:close(S), gen_tcp:close(L), B end, \
         R = [{}], \
         Sh = fun(Xs) -> lists:flatten(lists:join(\",\", [integer_to_list(V) || V <- Xs])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [Sh(binary_to_list(B)) || B <- R]))])",
        calls.join(", ")
    )
}

/// The Erlang side of the **write**-path cross-check: `gen_tcp` ports with the same framing
/// options, each sending the same payload through the real `inet` stack while a raw peer reads
/// the bytes that actually reach the wire, printed as comma-separated byte lists, `|`-separated
/// between cases. `Total` is one whole frame, so the peer reads exactly one message.
fn oracle_write_expression(cases: &[(Vec<u8>, PacketType, usize)]) -> String {
    let calls: Vec<String> = cases
        .iter()
        .map(|(payload, packet, total)| {
            let bytes: Vec<String> = payload.iter().map(u8::to_string).collect();
            format!(
                "F({}, <<{}>>, {})",
                erl_packet_option(*packet),
                bytes.join(","),
                total
            )
        })
        .collect();
    format!(
        "F = fun(Pkt, Payload, Total) -> \
         {{ok,L}} = gen_tcp:listen(0, [binary,{{active,false}},{{reuseaddr,true}},Pkt]), \
         {{ok,P}} = inet:port(L), \
         Parent = self(), \
         spawn(fun() -> {{ok,C}} = gen_tcp:connect({{127,0,0,1}},P,[binary,{{active,false}}]), \
           B = case gen_tcp:recv(C, Total, 2000) of {{ok,X}} -> X; _ -> <<>> end, \
           Parent ! {{captured,B}}, gen_tcp:close(C) end), \
         {{ok,S}} = gen_tcp:accept(L, 2000), \
         ok = gen_tcp:send(S, Payload), \
         timer:sleep(150), gen_tcp:close(S), gen_tcp:close(L), \
         receive {{captured,B}} -> B after 3000 -> <<>> end end, \
         R = [{}], \
         Sh = fun(Xs) -> lists:flatten(lists:join(\",\", [integer_to_list(V) || V <- Xs])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [Sh(binary_to_list(B)) || B <- R]))])",
        calls.join(", ")
    )
}

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after a
/// loud skip message — when `erl` cannot be spawned or does not answer in time.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(
            Command::new("erl")
                .arg("-noshell")
                .arg("-eval")
                .arg(&code)
                .output(),
        );
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Parses the oracle's `a,b,c|d,e|…` payload dump into one byte vector per case.
fn parse_payload_dump(stdout: &str) -> Option<Vec<Vec<u8>>> {
    let line = stdout.lines().find(|line| line.contains('|'))?;
    let mut payloads = Vec::new();
    for field in line.trim().split('|') {
        let mut bytes = Vec::new();
        for part in field.split(',') {
            if part.is_empty() {
                continue;
            }
            bytes.push(part.trim().parse::<u8>().ok()?);
        }
        payloads.push(bytes);
    }
    Some(payloads)
}

/// The compatibility gate for the framing: the payload this driver delivers for each
/// `{packet, N}` must be byte-for-byte what real `gen_tcp` delivers for the same wire bytes.
///
/// The oracle is the system `erl` on `PATH`; if it is absent the check **skips loudly**, never
/// fails. (It is present in CI and here, so this actually runs.)
#[test]
fn the_frame_payloads_agree_with_the_gen_tcp_oracle() {
    let cases: Vec<(Vec<u8>, PacketType)> = vec![
        (vec![0, 0, 0, 4, b'a', b'b', b'c', b'd'], PacketType::Len4),
        (vec![3, b'x', b'y', b'z'], PacketType::Len1),
        (vec![0, 3, b'h', b'i', b'j'], PacketType::Len2),
        (b"abc\n".to_vec(), PacketType::LineLf),
    ];

    // What this driver produces.
    let mut rust = Vec::new();
    for (wire, packet) in &cases {
        let mut poller = poller();
        let (mut registry, id, mut client) = connected_port(&mut poller);
        set_packet(&mut registry, &mut poller, id, *packet);
        client.write_all(wire).expect("the peer writes the wire bytes");
        rust.push(recv_frame(&mut registry, &mut poller, id));
        registry.close(&mut poller, id).expect("closes the port");
    }

    // What real `gen_tcp` produces for the same wire bytes.
    let Some(stdout) = erl_stdout(&oracle_expression(&cases)) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_payload_dump(&stdout) else {
        panic!("could not parse the oracle payload dump: {stdout:?}");
    };

    eprintln!("oracle payloads: {oracle:?}");
    eprintln!("driver payloads: {rust:?}");
    assert_eq!(oracle.len(), cases.len(), "one payload per case: {oracle:?}");

    for (index, ((wire, packet), (rust_payload, oracle_payload))) in
        cases.iter().zip(rust.iter().zip(oracle.iter())).enumerate()
    {
        assert_eq!(
            rust_payload, oracle_payload,
            "case {index} ({packet:?}, wire {wire:?}) disagrees with gen_tcp"
        );
    }
}

/// The **compatibility gate for the write path**: the bytes this driver puts on the wire for a
/// `send` must be byte-for-byte what real `gen_tcp:send` puts there for the same payload under
/// the same `{packet, N}` option.
///
/// The oracle runs the real `inet` stack (the C's `tcp_sendv`) with a raw peer reading exactly
/// one frame, so both sides are compared on the wire — the header included, which is what a
/// peer's `packet_get_length` relies on. The `{packet, 4}` empty-payload case pins the C's
/// "a framing code still writes the bare header" detail (`inet_drv.c:13907`).
///
/// The oracle is the system `erl` on `PATH`; if it is absent the check **skips loudly**, never
/// fails.
#[test]
fn the_wire_bytes_of_a_framed_send_agree_with_the_gen_tcp_oracle() {
    // (payload, packet code, the whole frame's length on the wire)
    let cases: Vec<(Vec<u8>, PacketType, usize)> = vec![
        (b"abc".to_vec(), PacketType::Len1, 4),
        (b"abc".to_vec(), PacketType::Len2, 5),
        (b"abc".to_vec(), PacketType::Len4, 7),
        (Vec::new(), PacketType::Len4, 4),
        (b"abc".to_vec(), PacketType::Raw, 3),
    ];

    // What this driver puts on the wire.
    let mut rust = Vec::new();
    for (payload, packet, total) in &cases {
        let mut poller = poller();
        let (mut registry, id, mut client) = connected_port(&mut poller);
        set_packet(&mut registry, &mut poller, id, *packet);
        rust.push(send_and_capture(
            &mut registry,
            &mut poller,
            id,
            &mut client,
            *total,
            payload,
        ));
        registry.close(&mut poller, id).expect("closes the port");
    }

    // What real `gen_tcp:send` puts there for the same payload.
    let Some(stdout) = erl_stdout(&oracle_write_expression(&cases)) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_payload_dump(&stdout) else {
        panic!("could not parse the oracle wire dump: {stdout:?}");
    };

    eprintln!("oracle wire bytes: {oracle:?}");
    eprintln!("driver wire bytes: {rust:?}");
    assert_eq!(oracle.len(), cases.len(), "one frame per case: {oracle:?}");

    for (index, ((payload, packet, total), (rust_wire, oracle_wire))) in
        cases.iter().zip(rust.iter().zip(oracle.iter())).enumerate()
    {
        assert_eq!(
            rust_wire.len(),
            *total,
            "case {index} ({packet:?}) is not one whole frame: {rust_wire:?}"
        );
        assert_eq!(
            rust_wire, oracle_wire,
            "case {index} ({packet:?}, payload {payload:?}) disagrees with gen_tcp"
        );
    }
}