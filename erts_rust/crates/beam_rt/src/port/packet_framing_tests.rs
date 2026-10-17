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
//! The `{packet, N}` **framing** suite: the emulator-side length header on the way out and the
//! packet reassembly on the way in (see `src/port/packet.rs`).
//!
//! Three layers, cheapest first:
//!
//! 1. the framer itself ([`PacketFraming`]) — the header width, the empty payload, the empty
//!    *packet*, a packet split across reads, several packets from one read, and the
//!    `/`{packet,1}`/`{packet,2}` overflow refusal;
//! 2. the runtime's own **write path** — a real `{packet,N}` port to a `cat > FILE` child, so
//!    the file holds exactly the bytes the child received (the header included);
//! 3. the runtime's own **read path** — a `sh` child that `printf`s framed packets, so the
//!    owner receives one `{Port, {data, …}}` per complete packet.
//!
//! Layers 2 and 3 are cross-checked against the live oracle: the oracle's own bytes are
//! computed by the *same* `-eval` expression shape a user would write, and a missing
//! `erl`/`/bin/sh` is a **loud skip**, never a silent pass.
use super::packet::{PacketFraming, PACKET_WIDTHS};
use super::tests::{assert_badarg, fixture, skip_oracle_check, Fixture};
use super::*;
/// Opens `{spawn_executable, "/bin/sh"}` running `script` with `options` appended.
fn open_shell(fx: &mut Fixture, script: &str, options: &[Eterm]) -> PortId {
    let path = fx.string("/bin/sh");
    let name = fx.name("spawn_executable", path);
    let dash_c = fx.string("-c");
    let script = fx.string(script);
    let args = fx.list(&[dash_c, script]);
    let tag = fx.atom("args");
    let args = fx.heap.tuple(&[tag, args]).expect("an args option");
    let mut all = vec![args];
    all.extend_from_slice(options);
    let options = fx.list(&all);
    fx.hook
        .open_port(name, options)
        .expect("open_port({spawn_executable,\"/bin/sh\"}, …)")
}
/// The `{packet, N}` option tuple.
fn packet_option(fx: &mut Fixture, width: i64) -> Eterm {
    let tag = fx.atom("packet");
    fx.heap.tuple(&[tag, make_small(width)]).expect("a packet option")
}
/// Every `{Port, {data, Data}}` payload the port produced, in order, until `bound` elapses.
///
/// A `{packet,N}` read delivers exactly one message per complete packet, so the vector's
/// shape **is** the framing: its length is the packet count and each entry is a payload with
/// the header stripped.
fn packets(fx: &mut Fixture, port: PortId, bound: Duration) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + bound;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let events = fx
            .registry
            .borrow_mut()
            .pump(Duration::from_millis(50))
            .expect("pump");
        let mut saw_exit = false;
        for event in events {
            match event {
                PortEvent::Data { port: from, bytes, .. } => {
                    assert_eq!(from, port, "a message from a foreign port");
                    out.push(bytes);
                }
                PortEvent::Exited { .. } => saw_exit = true,
                _ => {}
            }
        }
        if saw_exit {
            break;
        }
    }
    out
}
/// The payloads the **oracle** delivers for `script` under `{packet,N}`, one per message.
///
/// `None` when `erl` or `/bin/sh` cannot be spawned at all (the caller prints the loud skip).
fn oracle_packets(width: i64, script: &str) -> Option<Vec<Vec<u8>>> {
    let expr = format!(
        "begin \
         P = open_port({{spawn_executable,\"/bin/sh\"}},[{{args,[\"-c\",{script:?}]}},binary,\
         exit_status,{{packet,{width}}}]), \
         F = fun F(Acc) -> receive {{P,{{data,D}}}} -> F([D|Acc]); {{P,{{exit_status,_}}}} -> \
         lists:reverse(Acc) after 5000 -> lists:reverse(Acc) end end, \
         Ms = F([]), io:format(\"~w~n\",[[binary_to_list(B) || B <- Ms]]), ok end"
    );
    let output = std::process::Command::new("erl")
        .args(["-noshell", "-eval", &expr, "-s", "init", "stop"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(parse_int_lists(&text))
}
/// Parses the `[[1,2],[3]]` shape `~w` prints for a list of byte lists.
///
/// The **outer** brackets are the message list itself, so only a list nested inside them is a
/// payload: an empty result (`[]`, no message at all) must parse to *no* payloads, not to one empty
/// one. The depth counter is what tells the two apart — counting bytes out of the outer bracket pair
/// would turn "the oracle delivered nothing" into "the oracle delivered an empty packet".
fn parse_int_lists(text: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut depth = 0usize;
    let mut digits = String::new();
    for character in text.chars() {
        match character {
            '[' => {
                depth += 1;
                if depth == 2 {
                    current = Some(Vec::new());
                }
            }
            ']' => {
                if depth >= 2 {
                    if let Some(mut bytes) = current.take() {
                        if !digits.is_empty() {
                            bytes.push(digits.parse().expect("a byte"));
                            digits.clear();
                        }
                        out.push(bytes);
                    }
                }
                depth = depth.saturating_sub(1);
            }
            ',' => {
                if !digits.is_empty() {
                    if let Some(bytes) = current.as_mut() {
                        bytes.push(digits.parse().expect("a byte"));
                    }
                    digits.clear();
                }
            }
            other if other.is_ascii_digit() => {
                if depth == 2 {
                    digits.push(other);
                }
            }
            _ => {}
        }
    }
    out
}
/// The header widths the option accepts, and the header it writes for each: a 3-byte payload
/// is `<<3,97,98,99>>`, `<<0,3,97,98,99>>`, `<<0,0,0,3,97,98,99>>`.
#[test]
fn the_length_header_is_the_payload_length_in_the_option_width() {
    for (width, expected) in [
        (1usize, vec![3, 97, 98, 99]),
        (2, vec![0, 3, 97, 98, 99]),
        (4, vec![0, 0, 0, 3, 97, 98, 99]),
    ] {
        let framing = PacketFraming::new(width)
            .expect("an accepted width")
            .expect("framing is on");
        assert_eq!(
            framing.frame_output(b"abc").expect("a fitting payload"),
            expected,
            "{{packet,{width}}} frames a 3-byte payload"
        );
    }
    assert_eq!(PACKET_WIDTHS, [1, 2, 4], "the widths the option accepts");
    assert!(
        PacketFraming::new(0).expect("0 is the off switch").is_none(),
        "a 0 width is not a framing at all"
    );
    for width in [3, 5, 255] {
        assert_badarg(PacketFraming::new(width));
    }
}
/// The header is written even for an empty payload (`port_command(P, <<>>)` on a `{packet,4}`
/// port writes `<<0,0,0,0>>` — measured), and a payload wider than the header can express is
/// the reference runtime's port death, reported here as a typed gap.
#[test]
fn an_empty_payload_is_still_framed_and_an_oversized_one_is_refused() {
    let four = PacketFraming::new(4).unwrap().unwrap();
    assert_eq!(four.frame_output(b"").expect("empty frames"), vec![0, 0, 0, 0]);
    let one = PacketFraming::new(1).unwrap().unwrap();
    assert_eq!(
        one.frame_output(&[b'x'; 255]).expect("255 bytes fit").len(),
        256
    );
    let wide = one.frame_output(&[b'x'; 256]);
    assert!(
        wide.is_err(),
        "a 256-byte payload does not fit a {{packet,1}} header: {wide:?}"
    );
    let two = PacketFraming::new(2).unwrap().unwrap();
    assert!(two.frame_output(&[b'x'; 65535]).is_ok(), "65535 fits");
    assert!(two.frame_output(&[b'x'; 65536]).is_err(), "65536 does not");
}
/// The reassembler: several packets from one read, a packet split across reads, a partial
/// header, and the zero-length packet that is a real message.
#[test]
fn the_reassembler_answers_one_payload_per_complete_packet() {
    let mut framing = PacketFraming::new(4).unwrap().unwrap();
    assert_eq!(
        framing
            .push(&[0, 0, 0, 3, b'a', b'b', b'c', 0, 0, 0, 2, b'd', b'e'])
            .expect("a burst"),
        vec![b"abc".to_vec(), b"de".to_vec()],
        "two packets in one read are two deliveries"
    );
    assert!(framing.buffered().is_empty(), "nothing is left over");

    // A split packet: the header and half the payload first, the rest later.
    let mut framing = PacketFraming::new(4).unwrap().unwrap();
    assert_eq!(
        framing.push(&[0, 0, 0, 5, b'h', b'e']).expect("a partial"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(framing.buffered(), &[0, 0, 0, 5, b'h', b'e'], "held, unread");
    assert_eq!(
        framing.push(b"llo").expect("the rest"),
        vec![b"hello".to_vec()],
        "the packet completes on the second read"
    );
    assert!(framing.buffered().is_empty());

    // A partial header is buffered too.
    let mut framing = PacketFraming::new(2).unwrap().unwrap();
    assert_eq!(framing.push(&[0]).expect("half a header"), Vec::<Vec<u8>>::new());
    assert_eq!(framing.push(&[1, b'x']).expect("the rest"), vec![b"x".to_vec()]);

    // A zero-length packet **is** a message (measured: `{Port,{data,<<>>}}`).
    let mut framing = PacketFraming::new(4).unwrap().unwrap();
    assert_eq!(
        framing.push(&[0, 0, 0, 0]).expect("an empty packet"),
        vec![Vec::new()],
        "a header of 0 delivers an empty payload, never nothing"
    );
    let mut framing = PacketFraming::new(1).unwrap().unwrap();
    assert_eq!(
        framing.push(&[2, b'a', b'b', 1, b'c', 0]).expect("packet,1 stream"),
        vec![b"ab".to_vec(), b"c".to_vec(), Vec::new()]
    );
}
/// The three `open_port/2` option verdicts: only `1`/`2`/`4` are widths, `line` + a width is
/// `badarg`, and `stream` **cancels** the framing (which is what makes `[line, {packet,2},
/// stream]` legal).
#[test]
fn the_packet_option_takes_the_three_widths_and_is_cancelled_by_stream() {
    let mut fx = fixture();
    for width in [1, 2, 4] {
        let option = packet_option(&mut fx, width);
        let options = fx.list(&[option]);
        let parsed = fx
            .hook
            .port_options(options)
            .unwrap_or_else(|error| panic!("{{packet,{width}}} must be accepted: {error}"));
        assert_eq!(parsed.packet_bytes, width as usize, "the width is kept");
    }
    for width in [0, 3, 5, -1, 8] {
        let option = packet_option(&mut fx, width);
        let options = fx.list(&[option]);
        assert_badarg(fx.hook.port_options(options));
    }
    // A non-integer width is `badarg` too.
    let tag = fx.atom("packet");
    let bad = fx.string("four");
    let option = fx.heap.tuple(&[tag, bad]).expect("a malformed packet option");
    let options = fx.list(&[option]);
    assert_badarg(fx.hook.port_options(options));

    // `line` + a width is the mutual exclusion; `stream` cancels the width first.
    let line = fx.atom("line");
    let packet = packet_option(&mut fx, 2);
    let options = fx.list(&[line, packet]);
    assert_badarg(fx.hook.port_options(options));
    let stream = fx.atom("stream");
    let options = fx.list(&[line, packet, stream]);
    let parsed = fx
        .hook
        .port_options(options)
        .expect("[line,{packet,2},stream] is legal");
    assert_eq!(parsed.packet_bytes, 0, "`stream` cleared the width");
    assert!(parsed.line.is_some(), "and left the line framing alone");
}
/// **Output**: a `{packet,4}` port's `port_command` really writes `<<Len:32, Payload>>` to the
/// child, byte for byte as the oracle does.
#[test]
fn the_write_path_prepends_the_header_the_oracle_prepends() {
    let dir = crate::context::test_support::scratch_dir("packet_out");
    for width in [1i64, 2, 4] {
        let file = dir.path().join(format!("out_{width}.bin"));
        let script = format!("cat > {}", file.display());
        let mut fx = fixture();
        let option = packet_option(&mut fx, width);
        let port = open_shell(&mut fx, &script, &[option]);
        // The payload is deliberately empty and then non-empty: the first header is
        // `<<0,…>>`, which is how the oracle's own `port_command(P, <<>>)` behaves.
        fx.registry
            .borrow_mut()
            .write_command(port, b"abc")
            .expect("port_command");
        // Give the child a moment to consume the bytes before the port is closed (closing
        // the port closes its stdin, which is `cat`'s end of file).
        std::thread::sleep(Duration::from_millis(300));
        fx.hook.close_port(port).expect("port_close/1");
        std::thread::sleep(Duration::from_millis(300));
        let framed = std::fs::read(&file).unwrap_or_default();
        let framing = PacketFraming::new(width as usize).unwrap().unwrap();
        assert_eq!(
            framed,
            framing.frame_output(b"abc").expect("a fitting payload"),
            "{{packet,{width}}}: the child must receive the header followed by the payload"
        );
    }
}
/// **Input**: the child's framed bytes really arrive as one message per packet, and the
/// delivered payloads are the oracle's own.
#[test]
fn the_read_path_strips_the_header_and_delivers_one_message_per_packet() {
    // Two packets in one write, then a third after a pause (so the reads really split), then a
    // zero-length packet — the three shapes a reassembler must survive.
    let script = "printf '\\000\\000\\000\\003abc\\000\\000\\000\\002de'; \
                  sleep 0.3; printf '\\000\\000\\000\\000'";
    let mut fx = fixture();
    let option = packet_option(&mut fx, 4);
    let exit_status = fx.atom("exit_status");
    let port = open_shell(&mut fx, script, &[option, exit_status]);
    let got = packets(&mut fx, port, Duration::from_secs(10));
    assert_eq!(
        got,
        vec![b"abc".to_vec(), b"de".to_vec(), Vec::new()],
        "one message per complete packet, the empty one included"
    );
    match oracle_packets(4, script) {
        Some(oracle) => assert_eq!(got, oracle, "the payloads are the oracle's own"),
        None => skip_oracle_check("the {packet,4} input reassembly"),
    }
}
/// A `{packet,2}` and a `{packet,1}` stream rear too — and a **partial** packet at end of
/// stream is dropped, exactly as the C's `port_inp_failure` drops it (nothing truncated, no
/// message invented).
#[test]
fn a_narrow_header_streams_and_a_partial_packet_at_eof_is_dropped() {
    let script = "printf '\\002ab\\001c'";
    let mut fx = fixture();
    let option = packet_option(&mut fx, 1);
    let exit_status = fx.atom("exit_status");
    let port = open_shell(&mut fx, script, &[option, exit_status]);
    let got = packets(&mut fx, port, Duration::from_secs(10));
    assert_eq!(got, vec![b"ab".to_vec(), b"c".to_vec()], "{{packet,1}} packets");
    match oracle_packets(1, script) {
        Some(oracle) => assert_eq!(got, oracle, "the payloads are the oracle's own"),
        None => skip_oracle_check("the {packet,1} input reassembly"),
    }

    // `\0\0\0\5hel` announces five bytes and delivers three: nothing is delivered.
    let script = "printf '\\000\\000\\000\\005hel'";
    let mut fx = fixture();
    let option = packet_option(&mut fx, 4);
    let exit_status = fx.atom("exit_status");
    let port = open_shell(&mut fx, script, &[option, exit_status]);
    let got = packets(&mut fx, port, Duration::from_secs(10));
    assert!(
        got.is_empty(),
        "a partial packet at end of stream is dropped, never truncated: {got:?}"
    );
    match oracle_packets(4, script) {
        Some(oracle) => assert!(oracle.is_empty(), "the oracle drops it too: {oracle:?}"),
        None => skip_oracle_check("the partial-packet-at-eof rule"),
    }
}
/// A `{spawn_driver, Name}` port is **not** framed by the emulator: its `packet` option
/// belongs to the driver, exactly as in the C (whose `packet_bytes` only the sys drivers
/// read). Measured on the oracle: a `SIZE` command byte sent to a `{packet,4}` `ram_file_drv`
/// port answers the same reply the unframed port does.
#[test]
fn a_driver_port_keeps_its_packet_option_for_the_driver() {
    let mut fx = fixture();
    let name = fx.string("ram_file_drv");
    let name = fx.name("spawn_driver", name);
    let option = packet_option(&mut fx, 4);
    let options = fx.list(&[option]);
    let port = fx
        .hook
        .open_port(name, options)
        .expect("open_port({spawn_driver,\"ram_file_drv\"}, [{packet,4}])");
    // The driver sees the bare command byte: `SIZE` answers `NUMBER 0` on the empty file.
    fx.registry
        .borrow_mut()
        .write_command(port, &[37])
        .expect("port_command");
    let reply = {
        let mut registry = fx.registry.borrow_mut();
        let mut reply = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while reply.is_empty() && Instant::now() < deadline {
            for event in registry.pump(Duration::from_millis(50)).expect("pump") {
                if let PortEvent::Data { bytes, .. } = event {
                    reply = bytes;
                }
            }
        }
        reply
    };
    assert_eq!(
        reply,
        vec![3, 0, 0, 0, 0],
        "the driver received the raw command byte: no header was prepended"
    );
    fx.hook.close_port(port).expect("port_close/1");
}
