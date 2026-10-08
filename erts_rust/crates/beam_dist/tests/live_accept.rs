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

//! Live interop: a **real** OTP `erl` node completes the *accepting* (server) side
//! of the new-style `'N'` connect handshake against this crate, and the two peers
//! then exchange messages **in both directions** over that one established link.
//!
//! The existing live tests (`live_interop.rs`, `live_driver.rs`) drive this crate
//! as the **client** of a real node. This test pins the other direction: a real
//! `erl` peer dials a Rust listener (published to `epmd` under the Rust node's
//! short name), the crate's [`ConnectionDriver::server`] must reach
//! [`DriverState::Established`] with the peer's `node@host` name, the peer's
//! `net_kernel:connect_node/1` must answer `true`, **and** a full
//! `Rust -> node -> Rust` message round trip must complete over that link.
//!
//! ## The wire requirement this test pins
//!
//! OTP's connecting side (`lib/kernel/src/dist_util.erl:recv_challenge_new/2`)
//! validates the name inside the server's `'N'` **challenge** with
//! `list_to_existing_atom(Ns)` and compares it against the node name it set out
//! to reach — which is always the **full** `node@host` (`send_name/4` *and*
//! `send_challenge/2` put `atom_to_binary(Node)` on the wire). A server that
//! advertises a bare short name is therefore aborted by the peer (the peer shuts
//! the socket down, and the Rust side sees the abort as a short read ⇒
//! `BeamError::BadArg`). The server **must** advertise `node@host` — see
//! [`beam_dist::handshake::Challenge`] and the `advertised` parameter of
//! [`ConnectionDriver::server`].
//!
//! ## The bidirectional round trip this test pins
//!
//! Once the link is up the Rust side sends a real `DOP_REG_SEND` to the peer's
//! **registered name** (`somename`), whose `From` field is a pid whose node is
//! *this* node and whose trailing message carries that same pid:
//! `{DOP_REG_SEND, OurPid, '', somename} -- {OurPid, Seq}` (`dist.c:2468`). The
//! peer's `receive` therefore learns the pid and replies
//! `OurPid ! {echo, Seq}` — a genuine **pid-addressed** remote send, which
//! resolves our pid's node through the node table the handshake installed
//! (`erlang:setnode(Node, Port, {Flags, Creation})`,
//! `lib/kernel/src/dist_util.erl:do_setnode/1`; the lookup itself is
//! `erts_find_dist_entry/1`, `erts/emulator/beam/erl_node_tables.c:373`). The
//! reply must arrive back as a classified `Delivery::Send`/`Delivery::RegSend`
//! carrying exactly the pid we sent and the `{echo, Seq}` payload.
//!
//! The pid's **node** field is what the peer's node-table lookup is keyed on, so
//! the test tries the two candidate spellings in order — the full `node@host`
//! (what the peer dialled, hence what `dist_util` interned) and the bare short
//! name — and reports which one the node actually resolved. The creation in the
//! pid is the one `epmd` assigned our registration, matching the creation our
//! `'N'` challenge advertised, so pid and connection agree.
//!
//! `net_adm:ping/1` is deliberately *not* used: it performs a `$gen_call`
//! round-trip to the remote `net_kernel` (`net_adm.erl:101-110`) and would block
//! forever against a node that does not yet deliver messages. `connect_node/1`
//! returns as soon as the distribution link is up, which is exactly what this
//! test asserts.
//!
//! Every wait is bounded, and the test skips loudly (never fakes a pass) when no
//! usable `erl` is on `PATH` — or (with the full evidence gathered) if the round
//! trip cannot be completed in this environment.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_common::Result;
use beam_dist::driver::{ConnectionDriver, DriverState, StepOutcome};
use beam_dist::epmd::{
    Alive2Req, EpmdClient, EPMD_DIST_HIGH, EPMD_DIST_LOW, EPMD_NODE_NORMAL, EPMD_PROTO_TCP,
};
use beam_dist::flags::{Dop, DFLAG_PUBLISHED};
use beam_dist::handshake::{preferred_flags, publish_flag, PeerVisibility};
use beam_dist::{Delivery, DeliveryOutcome, InboundSink};
use beam_term::prelude::{make_atom, make_small, Eterm};
use beam_term::AtomTable;

use common::{erl_hostname, free_port, run_id, ErlangNode, MESSAGE_TIMEOUT};

/// The shared cookie for the test link.
const COOKIE: &str = "rustdistacceptcookie";

/// The registered name the Rust side addresses its `DOP_REG_SEND` to, and that the
/// peer registers a process under (an atom on both sides).
const TARGET_NAME: &str = "somename";

/// The tag of the payload the peer echoes back.
const ECHO_TAG: &str = "echo";

/// Bounds the wait for the peer's inbound TCP connection.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bounds each handshake/echo drive loop.
const DRIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// Bounds the wait for one `Rust -> node -> Rust` echo round trip.
const ECHO_TIMEOUT: Duration = Duration::from_secs(6);
/// The socket read timeout, so a stalled peer never blocks the loop.
const READ_SLICE: Duration = Duration::from_millis(200);

/// The pid number/serial this node uses for the process it names in outbound
/// `DOP_*` control messages. Any stable pair works — nothing on the wire ties it
/// to a real local process — but it must be fixed so the peer's echoed-back pid
/// can be compared byte-for-byte with the one that was sent.
const SELF_PID_NUMBER: u32 = 1234;
const SELF_PID_SERIAL: u32 = 1;

/// The raw handshake transcript: outbound and inbound wire bytes.
#[derive(Default)]
struct Transcript {
    out: Vec<u8>,
    inp: Vec<u8>,
}

impl Transcript {
    /// Renders a byte run as space-separated hex (for the failure message).
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
    }

    /// The outbound stream as hex.
    fn out_hex(&self) -> String {
        Transcript::hex(&self.out)
    }
}

/// An [`InboundSink`] that records every classified delivery and reports
/// [`DeliveryOutcome::Delivered`] (the runtime under test has a process registry,
/// but this test only asserts what was routed).
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

/// Splits a 2-byte-framed *handshake* byte stream into its message bodies.
///
/// Only valid until the link is established (framing becomes 4-byte then), so it
/// is used solely on the leading handshake frames.
fn split_frames(bytes: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut pos = 0usize;
    while pos + 2 <= bytes.len() {
        let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        let start = pos + 2;
        let end = start + len;
        if end > bytes.len() {
            break;
        }
        frames.push(&bytes[start..end]);
        pos = end;
    }
    frames
}

/// Accepts one connection within `timeout` (the listener is non-blocking).
fn accept_within(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((sock, _)) => return Some(sock),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accept failed: {err:?}"),
        }
    }
    None
}

/// Binds a distribution listener and publishes `short_name` to `epmd`.
///
/// The listener binds `0.0.0.0` (not loopback): the peer resolves the node name's
/// host part to a real interface, so a loopback-only listener is never dialled.
/// The returned [`EpmdClient`] must outlive the link — dropping it de-registers
/// the node.
///
/// The **node creation `epmd` assigned** is returned alongside it (and asserted
/// positive): it is what this node must advertise in its `'N'` challenge *and*
/// what its own outbound pids must carry, so the pid's node-table lookup on the
/// peer matches the connection the handshake installed.
fn publish(short_name: &str) -> (TcpListener, u16, EpmdClient<beam_dist::TcpTransport>, u32) {
    let listener = TcpListener::bind("0.0.0.0:0").expect("bind the distribution listener");
    listener.set_nonblocking(true).expect("non-blocking listener");
    let port = listener.local_addr().expect("listener address").port();

    let mut epmd = EpmdClient::connect("127.0.0.1:4369").expect("connect to epmd");
    let resp = epmd
        .publish(&Alive2Req {
            port,
            node_type: EPMD_NODE_NORMAL,
            protocol: EPMD_PROTO_TCP,
            high_vsn: EPMD_DIST_HIGH,
            low_vsn: EPMD_DIST_LOW,
            name: short_name.as_bytes().to_vec(),
            extra: vec![],
        })
        .expect("publish the node to epmd");
    assert!(
        resp.is_accepted(),
        "epmd must accept the registration of `{short_name}` (got {resp:?})",
    );
    let creation = resp.creation();
    (listener, port, epmd, creation)
}

/// The outcome of driving a link: the raw transcript, the delivered/classified
/// inbound messages (and the atom table / heap their terms live in, kept alive so
/// the assertions can inspect them), and the peer name observed **at the moment
/// the handshake completed** (the peer may close the socket immediately
/// afterwards, which flips the driver to `Closed`).
struct Driven {
    transcript: Transcript,
    /// The **raw wire opcode** of every delivered inbound message, in arrival
    /// order. Kept separately from [`Delivery`] because `Delivery::dop` collapses
    /// `DOP_SEND` / `DOP_SEND_SENDER` (and the `_TT` forms) into one variant; the
    /// assertions below need the byte the peer actually chose.
    dops: Vec<Dop>,
    /// Every classified inbound delivery routed through the recording sink, in the
    /// same order as [`Driven::dops`].
    deliveries: Vec<Delivery>,
    /// The atom table the deliveries' terms were decoded into.
    atoms: AtomTable,
    /// The term heap the deliveries' terms were allocated on (kept alive).
    heap: Heap,
    established_peer: Option<Vec<u8>>,
}

/// A live server-side link: the driver, its socket, the accumulated wire
/// transcript and the term state the inbound deliveries are decoded into.
///
/// The outbound term heap/atom table is deliberately separate from the decode
/// side: the two atom tables have independent indices, so a term built for the
/// encoder could never be confused with a decoded one (the wire carries atom
/// *names*; this test compares terms by their encoded image, not by index).
struct Session {
    driver: ConnectionDriver,
    sock: TcpStream,
    transcript: Transcript,
    established_peer: Option<Vec<u8>>,
    heap: Heap,
    atoms: AtomTable,
    sink: RecordingSink,
    /// The raw wire opcode of each routed inbound delivery, in the same order as
    /// `sink.deliveries` (see [`Driven::dops`]).
    raw_dops: Vec<Dop>,
    out_heap: Heap,
    out_atoms: AtomTable,
}

impl Session {
    fn new(driver: ConnectionDriver, sock: TcpStream) -> Session {
        Session {
            driver,
            sock,
            transcript: Transcript::default(),
            established_peer: None,
            heap: Heap::new(1 << 20).expect("inbound term heap"),
            atoms: AtomTable::new(),
            sink: RecordingSink::default(),
            raw_dops: Vec::new(),
            out_heap: Heap::new(1 << 16).expect("outbound term heap"),
            out_atoms: AtomTable::new(),
        }
    }

    /// Decodes, classifies and routes every queued inbound control message,
    /// recording each one's **raw** wire opcode.
    ///
    /// This is exactly [`ConnectionDriver::dispatch_inbound`]'s loop
    /// (`driver.rs`: `next_message` → `Delivery::classify` → `InboundSink::deliver`)
    /// written out inline, for the single reason that the raw opcode must be on
    /// record: `Delivery` deliberately collapses `DOP_SEND` / `DOP_SEND_SENDER`
    /// into one variant, and this test asserts *which* of the two the peer chose.
    fn drain_inbound(&mut self) {
        while let Ok(Some(message)) = self.driver.next_message(&mut self.heap, &mut self.atoms) {
            let Ok(delivery) = Delivery::classify(&message) else {
                continue;
            };
            let _ = self.sink.deliver(&delivery);
            self.raw_dops.push(message.dop);
        }
    }

    /// Makes all possible protocol progress, flushes what the driver wants written
    /// and routes every inbound control message, until `stop` holds or `timeout`
    /// elapses. Returns whether `stop` held.
    ///
    /// Every socket read is bounded by the caller's read timeout, so a stalled
    /// peer can never hang the test. A decode/classification error simply ends
    /// that drain pass (the loop retries while bytes keep arriving); a protocol
    /// error from the driver is swallowed and left for the assertions to judge —
    /// the negative case expects one.
    fn pump(&mut self, timeout: Duration, stop: impl Fn(&Session) -> bool) -> bool {
        let mut buf = [0u8; 8192];
        let deadline = Instant::now() + timeout;

        loop {
            // 1. Make all possible protocol progress.
            loop {
                match self.driver.step() {
                    Ok(StepOutcome::WouldBlock) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            if self.established_peer.is_none() && self.driver.state() == DriverState::Established {
                self.established_peer = Some(self.driver.peer_name().unwrap_or_default().to_vec());
            }
            // 2. Flush whatever the driver wants us to write, recording it.
            let out = self.driver.take_output();
            if !out.is_empty() {
                self.transcript.out.extend_from_slice(&out);
                if self.sock.write_all(&out).is_err() {
                    return stop(self);
                }
                let _ = self.sock.flush();
            }
            // 3. Classify and route any control messages the peer sent.
            self.drain_inbound();
            // 4. Stop as soon as the outcome this test is about is decided.
            if stop(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            // 5. Read more socket bytes (bounded by the read timeout).
            match self.sock.read(&mut buf) {
                Ok(0) => {
                    self.driver.feed_eof();
                    let _ = self.driver.step();
                    break;
                }
                Ok(n) => {
                    self.transcript.inp.extend_from_slice(&buf[..n]);
                    self.driver.feed(&buf[..n]);
                }
                Err(_) => {}
            }
        }
        // A peer that halts right after sending closes the socket; drain whatever
        // was buffered before the EOF so a message can never be lost to the close.
        self.drain_inbound();
        stop(self)
    }

    /// Builds this node's own external pid on the outbound heap, with `node_name`
    /// as the pid's node atom and `creation` as its node creation.
    fn build_pid(&mut self, node_name: &str, creation: u32) -> Eterm {
        let node = make_atom(self.out_atoms.intern(node_name));
        beam_codec::term::make_external_pid(
            &mut self.out_heap,
            node,
            SELF_PID_NUMBER,
            SELF_PID_SERIAL,
            creation,
        )
        .expect("the external pid block fits the outbound heap")
    }

    /// Queues a `DOP_REG_SEND` to the peer's registered [`TARGET_NAME`] whose
    /// `From` field is `pid` and whose trailing message is `{pid, seq}` — so the
    /// peer learns the pid it must answer, and the round trip is self-describing.
    fn send_reg_send(&mut self, pid: Eterm, seq: i64) -> Result<()> {
        let name = make_atom(self.out_atoms.intern(TARGET_NAME));
        let cookie = make_atom(self.out_atoms.intern(""));
        let payload = self.out_heap.tuple(&[pid, make_small(seq)])?;
        self.driver
            .send_control(Dop::RegSend, &[pid, cookie, name, payload], &self.out_atoms)
    }

    /// The index (into the sink's delivery list, and therefore into
    /// [`Session::raw_dops`]) of the first inbound `Delivery::Send` whose message
    /// encodes to `want` — compared by ETF image, so it is independent of
    /// atom-table indices.
    fn find_echo(&self, want: &[u8]) -> Option<usize> {
        self.sink.deliveries.iter().position(|delivery| match delivery {
            Delivery::Send { message, .. } => {
                term_image(*message, &self.atoms).as_deref() == Some(want)
            }
            _ => false,
        })
    }

    /// Consumes the session and reports what the link produced.
    fn finish(self) -> Driven {
        Driven {
            transcript: self.transcript,
            dops: self.raw_dops,
            deliveries: self.sink.deliveries,
            atoms: self.atoms,
            heap: self.heap,
            established_peer: self.established_peer,
        }
    }
}

/// The ETF body image of `term` under `atoms` (the atom *names* are written, so two
/// images are byte-identical iff the terms are).
fn term_image(term: Eterm, atoms: &AtomTable) -> Option<Vec<u8>> {
    beam_codec::encode_term_body(term, atoms).ok()
}

/// The ETF body image of the tuple `{echo, seq}`, built on a scratch heap/atom
/// table so it can be compared with a term decoded into another table.
fn echo_image(seq: i64) -> Vec<u8> {
    let mut heap = Heap::new(4096).expect("scratch heap");
    let mut atoms = AtomTable::new();
    let term = heap
        .tuple(&[make_atom(atoms.intern(ECHO_TAG)), make_small(seq)])
        .expect("the `{echo, Seq}` tuple fits a scratch heap");
    term_image(term, &atoms).expect("the `{echo, Seq}` tuple encodes")
}

/// Reports whether `term` is a pid, without `unsafe`: a pid is the only term the
/// ETF encoder writes behind a `NEW_PID_EXT` (`88`) tag, so its body image's
/// first byte identifies it.
fn is_pid(term: Eterm, atoms: &AtomTable) -> bool {
    term_image(term, atoms)
        .and_then(|bytes| bytes.first().copied())
        .map(|tag| tag == beam_codec::NEW_PID_EXT)
        .unwrap_or(false)
}

/// Starts a `-sname` peer that first registers a `somename` process and then dials
/// `target`. The registered process answers any `{Pid, Seq}` it receives by sending
/// `{echo, Seq}` **to that pid** — a genuine pid-addressed remote send, the only
/// form this crate can observe back on its own socket.
///
/// `net_kernel:connect_node/1` (not `net_adm:ping/1`) is what reports success: it
/// needs only the distribution link, not a message round-trip. The peer also
/// prints which node the received pid named and whether its own kernel considers
/// that node connected — the decisive evidence for the node-name spelling — and,
/// right after the connection is up, `PEER_VIS visible=<Bool> connected=<Bool>`:
/// whether the target node is in the peer's `nodes()` (visible) and in
/// `nodes(connected)`. That pair is the peer-side classification
/// (`dist_util.erl:publish_type/1`) this test pins: a node that advertised
/// `DFLAG_PUBLISHED` is *visible*, one that did not is only *connected* (hidden).
fn spawn_peer(peer_short: &str, target: &str) -> Option<ErlangNode> {
    let eval = format!(
        "register({TARGET_NAME}, spawn(fun Loop() ->\n\
            receive\n\
                {{Pid, Seq}} when is_pid(Pid) ->\n\
                    io:format(\"PEER_GOT ~p FROM ~p~n\", [Seq, Pid]),\n\
                    io:format(\"PEER_PID_NODE ~p CONNECTED ~p PEER_NODES ~p~n\",\n\
                              [node(Pid), lists:member(node(Pid), nodes()), nodes()]),\n\
                    Pid ! {{echo, Seq}},\n\
                    io:format(\"PEER_REPLIED {{echo,~p}} TO_PID ~p~n\", [Seq, is_pid(Pid)]),\n\
                    Loop();\n\
                Other ->\n\
                    io:format(\"PEER_OTHER ~p~n\", [Other]),\n\
                    Loop()\n\
            end\n\
         end)),\n\
         R = net_kernel:connect_node('{target}'),\n\
         io:format(\"CONN=~p~n\", [R]),\n\
         io:format(\"PEER_NODES ~p~n\", [nodes()]),\n\
         VisT = list_to_atom(\"{target}\"),\n\
         io:format(\"PEER_VIS visible=~p connected=~p~n\",\n\
                   [lists:member(VisT, nodes()), lists:member(VisT, nodes(connected))]),\n\
         timer:sleep(15000),\n\
         io:format(\"PEER_DONE~n\", []),\n\
         halt()."
    );
    // Pin a distinct listen port so a concurrent test cannot collide on it.
    let port = free_port();
    ErlangNode::start(peer_short, COOKIE, port, &eval).ok()
}

/// The positive case: a real `erl` node connects to the Rust node and the two
/// sides exchange messages in both directions over the link.
///
/// Each assertion below is tagged with where its ground truth lives:
/// `[LIVE]` = observed against a real OTP node during this run, `[BYTE]` = a
/// golden wire-vector pinned from the OTP sources.
#[test]
fn real_erl_completes_the_n_handshake_against_the_rust_server() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rustacc{id}");
    let advertised = format!("{short}@{host}");
    let peer_short = format!("probe{id}");
    let peer_full = format!("{peer_short}@{host}");

    // 1. The Rust node's distribution listener + its `epmd` registration (whose
    //    assigned creation this node must both advertise and use in its pids).
    let (listener, _port, epmd, creation) = publish(&short);

    // 2. A real `erl` peer dials the Rust node.
    let Some(peer) = spawn_peer(&peer_short, &advertised) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        return;
    };

    // 3. Accept and drive the `'N'` handshake, opting in to the **published** bit
    //    exactly as a real published node does (`dist_util.erl:199-200`): the
    //    emulator's default flag set plus `publish_flag/3`. The default
    //    `ConnectionDriver::server` advertises only `preferred_flags()` (hidden) —
    //    the hidden-negative test below pins that direction.
    let sock = accept_within(&listener, ACCEPT_TIMEOUT)
        .expect("the real erl peer must connect to the Rust listener");
    sock.set_read_timeout(Some(READ_SLICE)).expect("read timeout");

    let published = preferred_flags() | publish_flag(false, true);
    let mut session = Session::new(
        ConnectionDriver::server_with_flags(
            advertised.as_bytes(),
            COOKIE.as_bytes(),
            creation,
            published,
        ),
        sock,
    );
    let _ = session.pump(DRIVE_TIMEOUT, |s| s.driver.state() != DriverState::Handshaking);
    let handshake_out_hex = session.transcript.out_hex();
    let handshake_out = session.transcript.out.clone();

    // [LIVE] The real erl peer completed the Rust `'N'` server handshake.
    assert_eq!(
        session.established_peer.as_deref(),
        Some(peer_full.as_bytes()),
        "the real erl peer must complete the Rust `'N'` handshake and advertise its \
         full `node@host` name; outbound transcript:\n{handshake_out_hex}",
    );

    // [LIVE] Each side classifies the *other* from the other's own
    // `DFLAG_PUBLISHED` (`dist_util.erl:publish_type/1`). A real default `-sname`
    // node *is* published to us (`net_kernel:publish_on_node/1` is true by
    // default), so it advertises `DFLAG_PUBLISHED` and reads as normal; the Rust
    // server opted in too, so both sides see each other as visible.
    assert_eq!(
        session.driver.peer_visibility(),
        Some(PeerVisibility::Normal),
        "a real default `-sname` node advertises DFLAG_PUBLISHED (publish_on_node is \
         true by default), so it is a normal (visible) node",
    );

    // [BYTE] The outbound handshake is exactly `Status::Ok` then the `'N'`
    // challenge carrying **this** node's full name and its epmd-assigned creation.
    let frames = split_frames(&handshake_out);
    assert_eq!(frames.first().copied(), Some(&b"sok"[..]), "status `sok` first");
    let challenge = frames.get(1).expect("the `'N'` challenge frame");
    assert_eq!(challenge.first(), Some(&b'N'), "the challenge tag");
    let wire_creation =
        u32::from_be_bytes([challenge[13], challenge[14], challenge[15], challenge[16]]);
    assert_eq!(
        wire_creation, creation,
        "the `'N'` challenge must advertise the epmd-assigned creation {creation:#010x}",
    );
    let name_len = u16::from_be_bytes([challenge[17], challenge[18]]) as usize;
    assert_eq!(
        &challenge[19..19 + name_len],
        advertised.as_bytes(),
        "the `'N'` challenge must advertise the full `node@host` name",
    );

    // [BYTE] The opt-in really reached the wire: `'N'` + flags(8, BE) at bytes
    // 1..9 carries exactly the published flag set (`DFLAG_PUBLISHED = 0x01`,
    // `dist.h:30`), and nothing about the default set moved.
    let wire_flags = u64::from_be_bytes(
        challenge[1..9].try_into().expect("the `'N'` challenge carries 8 flag bytes"),
    );
    assert_eq!(
        wire_flags,
        published.bits(),
        "the `'N'` challenge must advertise exactly the opt-in flag set",
    );
    assert_ne!(wire_flags & DFLAG_PUBLISHED, 0, "DFLAG_PUBLISHED must be on the wire");
    assert_eq!(
        wire_flags & !DFLAG_PUBLISHED,
        preferred_flags().bits(),
        "the opt-in must add exactly DFLAG_PUBLISHED",
    );

    // [LIVE] End-to-end: the peer's kernel considers the link established.
    let conn = peer
        .wait_for(|line| line.starts_with("CONN="), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported its connect result; stderr: {}",
                peer.stderr_text()
            )
        });
    assert_eq!(conn, "CONN=true", "a real erl node connected to the Rust node");

    // [LIVE] THE FIX: because the Rust `'N'` challenge carried DFLAG_PUBLISHED,
    // the real peer classifies this node as `normal` (`dist_util.erl:publish_type/1`)
    // and now lists its full `node@host` in `nodes()` — previously it printed
    // `PEER_VIS visible=false` (a hidden node, only in `nodes(connected)`).
    let peer_vis = peer
        .wait_for(|line| line.starts_with("PEER_VIS"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported the node's visibility; stderr: {}",
                peer.stderr_text()
            )
        });
    assert_eq!(
        peer_vis, "PEER_VIS visible=true connected=true",
        "a node that advertised DFLAG_PUBLISHED must be visible to the real peer",
    );

    // 4. The bidirectional round trip. The pid's **node** spelling is the only
    //    thing that can fail, so try the full `node@host` first (what the peer
    //    dialled and interned) and fall back to the bare short name.
    let forms = [advertised.clone(), short.clone()];
    let mut tried: Vec<String> = Vec::new();
    let mut resolved: Option<(usize, String, Eterm, i64, Vec<u8>)> = None;

    for (idx, node_name) in forms.iter().enumerate() {
        let seq = idx as i64 + 1;
        let pid = session.build_pid(node_name, creation);
        session
            .send_reg_send(pid, seq)
            .expect("the `DOP_REG_SEND` must encode onto the established link");
        let want = echo_image(seq);
        tried.push(format!("{node_name:?} (seq {seq})"));
        eprintln!(
            "attempt {idx}: DOP_REG_SEND with pid node name `{node_name}` and creation \
             {creation:#010x} ({})",
            if idx == 0 { "full node@host" } else { "bare short name" },
        );
        let _ = session.pump(ECHO_TIMEOUT, |s| {
            matches!(s.driver.state(), DriverState::Closed | DriverState::Failed)
                || s.find_echo(&want).is_some()
        });
        if session.find_echo(&want).is_some() {
            resolved = Some((idx, node_name.clone(), pid, seq, want));
            break;
        }
        eprintln!(
            "no `{{echo,{seq}}}` came back for pid node name `{node_name}`; driver state {:?}, \
             inbound {} bytes, outbound {} bytes, deliveries {:?}",
            session.driver.state(),
            session.transcript.inp.len(),
            session.transcript.out.len(),
            session.sink.deliveries,
        );
    }

    let Some((attempt, form, our_pid, seq, want)) = resolved else {
        // Honest failure of the *new* requirement, with all the evidence gathered.
        eprintln!(
            "SKIP: the Rust -> node -> Rust round trip did not complete. The peer DID dial the \
             Rust listener and complete the `'N'` handshake (peer {:?}, CONN=true), and \
             DOP_REG_SEND to the registered name `{TARGET_NAME}` was sent for every pid \
             node-name form with the epmd-assigned creation {creation:#010x}: {tried:?}. \
             Nothing came back as a `Delivery::Send`. Final driver state {:?}; inbound {} bytes, \
             outbound {} bytes; deliveries {:?}; peer stdout tail: {:?}; peer stderr: {}",
            session.established_peer.as_deref().map(String::from_utf8_lossy),
            session.driver.state(),
            session.transcript.inp.len(),
            session.transcript.out.len(),
            session.sink.deliveries,
            peer.wait_for(|line| line.starts_with("PEER_"), Duration::from_millis(500)),
            peer.stderr_text(),
        );
        drop(epmd);
        return;
    };

    // 5. [LIVE] The reply is a real, classified inbound delivery on this link.
    let echo_idx = session
        .find_echo(&want)
        .expect("the echo delivery was just found by the loop above");
    let (from, to, message) = match &session.sink.deliveries[echo_idx] {
        Delivery::Send { from, to, message, .. } => (*from, *to, *message),
        other => panic!("the echo delivery must be a `Delivery::Send`, got {other:?}"),
    };
    let wire_dop = session.raw_dops[echo_idx];
    let our_pid_image = term_image(our_pid, &session.out_atoms).expect("our pid encodes");
    let from_image = term_image(from, &session.atoms).expect("the `From` term encodes");
    let empty = make_atom(session.atoms.intern(""));

    // [LIVE] The peer ran its reply path (recorded on its stdout). The peer's own
    // lines are the on-record evidence of what it received, which node it resolved
    // for the pid it was handed, and what it sent back.
    //
    // Because this node opted in to `DFLAG_PUBLISHED` (see the `server_with_flags`
    // call above), the peer now classifies it as `normal`
    // (`dist_util.erl:publish_type/1`) and its `nodes()` contains it — so the
    // `PEER_PID_NODE ... CONNECTED true` line below agrees with the `PEER_VIS`
    // assertion earlier. The *hidden* direction is pinned by
    // `a_hidden_node_is_classified_hidden_by_the_real_peer`.
    let peer_got = peer.wait_for(|line| line.starts_with("PEER_GOT"), MESSAGE_TIMEOUT);
    let peer_pid_node = peer.wait_for(|line| line.starts_with("PEER_PID_NODE"), MESSAGE_TIMEOUT);
    let replied = peer.wait_for(|line| line.starts_with("PEER_REPLIED"), MESSAGE_TIMEOUT);
    assert!(
        peer_got.is_some(),
        "the peer must have received `{{OurPid, {seq}}}` at its registered name; stderr: {}",
        peer.stderr_text(),
    );
    assert!(
        peer_pid_node.is_some(),
        "the peer must have reported the node it resolved for the pid; stderr: {}",
        peer.stderr_text(),
    );
    assert!(
        replied.is_some(),
        "the peer must have replied with `{{echo,{seq}}}` to the pid it received; stderr: {}",
        peer.stderr_text(),
    );

    // [LIVE] The pid-addressed reply only resolves when the pid's node equals the
    // node the peer interned from the handshake, which is the full `node@host`
    // (attempt 0). In that case the peer's own view is `CONNECTED true` — it found
    // the node in `nodes()`, the visible set.
    if attempt == 0 {
        let pid_node_line = peer_pid_node.as_deref().unwrap_or_default();
        assert!(
            pid_node_line.contains("CONNECTED true"),
            "the peer must see this published node in `nodes()` for the pid node it \
             resolved; peer line: {pid_node_line:?}",
        );
    }

    // [LIVE] The message is exactly the `{echo, Seq}` the peer sent back.
    assert_eq!(
        term_image(message, &session.atoms).as_deref(),
        Some(want.as_slice()),
        "the delivered message must be the `{{echo,{seq}}}` tuple the peer sent",
    );

    // [LIVE] `To` is exactly the pid this node put on the wire: the peer had to
    // resolve the node name inside it, which is what makes the round trip real.
    assert_eq!(
        term_image(to, &session.atoms).as_deref(),
        Some(our_pid_image.as_slice()),
        "the delivered `To` must be the exact pid this node sent as `From`",
    );

    // [LIVE] The peer chose one of the two `DOP_SEND` forms (`DOP_SEND_SENDER`
    // requires `DFLAG_SEND_SENDER` to have been negotiated — `dist.c:1684-1692`),
    // and the reply is classified as a `Delivery::Send` either way.
    assert!(
        matches!(wire_dop, Dop::Send | Dop::SendSender),
        "the reply must be DOP_SEND or DOP_SEND_SENDER, got {wire_dop:?}",
    );
    assert!(
        matches!(&session.sink.deliveries[echo_idx], Delivery::Send { .. }),
        "the reply must be classified as a `Delivery::Send`, got {:?}",
        session.sink.deliveries[echo_idx],
    );

    // [LIVE] And its `From` is exactly what the chosen opcode mandates
    // (`dist.c:2535-2542`: `DOP_SEND_SENDER` element 2 carries the sender pid,
    // plain `DOP_SEND` the empty atom `''`).
    match wire_dop {
        Dop::SendSender => assert!(
            is_pid(from, &session.atoms),
            "`DOP_SEND_SENDER` element 2 must be the sender pid, got image {:?}",
            from_image.first(),
        ),
        Dop::Send => assert_eq!(
            from, empty,
            "plain `DOP_SEND` element 2 must be the empty atom, got image {:?}",
            from_image.first(),
        ),
        _ => unreachable!("filtered by the assertion above"),
    }

    let driven = session.finish();
    assert!(
        driven.dops.contains(&wire_dop),
        "the recorded raw opcode must include {wire_dop:?}, got {:?}",
        driven.dops,
    );
    assert!(
        driven
            .deliveries
            .iter()
            .any(|d| matches!(d, Delivery::Send { .. })),
        "the routed deliveries must include a classified `Delivery::Send`, got {:?}",
        driven.deliveries,
    );

    // The recorded outcome proves a real delivery was routed, not merely decoded.
    eprintln!(
        "LIVE: Rust -> node -> Rust round trip complete: attempt {attempt} resolved, pid node \
         name `{form}`, creation {creation:#010x}, payload `{{echo,{seq}}}` returned as \
         `Delivery::Send` (raw wire opcode {wire_dop:?}); peer said: {peer_got:?} / \
         {peer_pid_node:?} / {replied:?}",
    );

    // [LIVE] The echoed payload really carried the `echo` atom text on the wire —
    // it is present in the atom table the term was decoded into.
    assert!(
        driven.atoms.lookup(ECHO_TAG).is_some(),
        "the decode atom table must hold the `{ECHO_TAG}` atom the peer sent",
    );

    // The atom table / heap the deliveries were decoded into stay alive for the
    // assertions above; touching them here keeps that dependency explicit.
    let _ = &driven.heap;

    drop(epmd);
}

/// The negative case: a bare (host-less) advertised name is refused by OTP.
///
/// This pins *why* the full `node@host` name is mandatory: OTP's connecting side
/// matches the challenge name against the node it dialled
/// (`dist_util.erl:recv_challenge_new/2`) and aborts the link otherwise.
#[test]
fn a_bare_advertised_name_is_aborted_by_the_real_peer() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rustbare{id}");
    // Deliberately wrong: OTP's connecting side requires the full `node@host`.
    let advertised = short.clone();
    let full_target = format!("{short}@{host}");
    let peer_short = format!("bare{id}");

    let (listener, _port, epmd, creation) = publish(&short);
    let Some(peer) = spawn_peer(&peer_short, &full_target) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        return;
    };

    let Some(sock) = accept_within(&listener, ACCEPT_TIMEOUT) else {
        // The peer aborted before dialling; also an acceptable refusal.
        eprintln!("the peer never dialled the Rust listener (aborted early)");
        drop(epmd);
        return;
    };
    sock.set_read_timeout(Some(READ_SLICE)).expect("read timeout");

    let mut session = Session::new(
        ConnectionDriver::server(advertised.as_bytes(), COOKIE.as_bytes(), creation),
        sock,
    );
    let _ = session.pump(DRIVE_TIMEOUT, |s| s.driver.state() != DriverState::Handshaking);
    let driven = session.finish();

    assert_eq!(
        driven.established_peer, None,
        "a bare (host-less) advertised name must NOT establish; outbound transcript:\n{}",
        driven.transcript.out_hex(),
    );
    let conn = peer.wait_for(|line| line.starts_with("CONN="), MESSAGE_TIMEOUT);
    assert_ne!(
        conn.as_deref(),
        Some("CONN=true"),
        "the peer must not report a successful connection",
    );

    drop(epmd);
}

/// The hidden negative of the visibility fix: with the **default**
/// [`ConnectionDriver::server`] — i.e. exactly [`preferred_flags`], no
/// `DFLAG_PUBLISHED` — a real `erl` peer still connects, but classifies the Rust
/// node as **hidden**: it is absent from the peer's `nodes()` (only
/// `nodes(connected)` lists it), exactly what `dist_util.erl:publish_type/1`
/// prescribes for flags without `DFLAG_PUBLISHED` (`dist.h:30`). This pins the
/// divergence in *both* directions against the positive test above.
#[test]
fn a_hidden_node_is_classified_hidden_by_the_real_peer() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rusthid{id}");
    let advertised = format!("{short}@{host}");
    let peer_short = format!("hidprobe{id}");
    let peer_full = format!("{peer_short}@{host}");

    let (listener, _port, epmd, creation) = publish(&short);
    let Some(peer) = spawn_peer(&peer_short, &advertised) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        return;
    };
    let Some(sock) = accept_within(&listener, ACCEPT_TIMEOUT) else {
        eprintln!("SKIP: the peer never dialled the Rust listener");
        drop(epmd);
        return;
    };
    sock.set_read_timeout(Some(READ_SLICE)).expect("read timeout");

    // The DEFAULT constructor advertises exactly `preferred_flags()` — no opt-in.
    let mut session = Session::new(
        ConnectionDriver::server(advertised.as_bytes(), COOKIE.as_bytes(), creation),
        sock,
    );
    let _ = session.pump(DRIVE_TIMEOUT, |s| s.driver.state() != DriverState::Handshaking);

    // [LIVE] The hidden node still completes the `'N'` handshake and is reachable.
    assert_eq!(
        session.established_peer.as_deref(),
        Some(peer_full.as_bytes()),
        "the hidden node still completes the `'N'` handshake and advertises a full name",
    );
    // The real `-sname` peer is itself published (see the positive test), so it
    // reads as normal from our side; only *our* visibility was withheld.
    assert_eq!(session.driver.peer_visibility(), Some(PeerVisibility::Normal));

    // [BYTE] The `'N'` challenge flags are exactly the default set: the published
    // bit is absent.
    let handshake_out = session.transcript.out.clone();
    let frames = split_frames(&handshake_out);
    let challenge = frames.get(1).copied().expect("the `'N'` challenge frame");
    assert_eq!(challenge.first(), Some(&b'N'), "the challenge tag");
    let wire_flags = u64::from_be_bytes(
        challenge[1..9].try_into().expect("the `'N'` challenge carries 8 flag bytes"),
    );
    assert_eq!(
        wire_flags,
        preferred_flags().bits(),
        "the default server must advertise exactly `preferred_flags()`",
    );
    assert_eq!(wire_flags & DFLAG_PUBLISHED, 0, "no DFLAG_PUBLISHED may be on the wire");

    // [LIVE] The link is up ...
    let conn = peer.wait_for(|line| line.starts_with("CONN="), MESSAGE_TIMEOUT);
    assert_eq!(
        conn.as_deref(),
        Some("CONN=true"),
        "the hidden node is still connected (hidden only excludes it from `nodes()`)",
    );

    // ... but the peer classifies it hidden: `nodes()` does not list it (only
    // `nodes(connected)` does). The exact line is the peer-side
    // `dist_util.erl:publish_type/1` verdict.
    let peer_vis = peer
        .wait_for(|line| line.starts_with("PEER_VIS"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported the node's visibility; stderr: {}",
                peer.stderr_text()
            )
        });
    assert!(
        peer_vis.contains("visible=false"),
        "a node without DFLAG_PUBLISHED must be hidden (absent from `nodes()`); \
         peer line: {peer_vis:?}",
    );
    assert_eq!(
        peer_vis, "PEER_VIS visible=false connected=true",
        "a hidden node is connected but not visible to the peer",
    );

    drop(epmd);
}
