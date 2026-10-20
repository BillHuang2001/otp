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

//! **The wire itself** (contract A.5): the `DOP_MONITOR_P_EXIT` control a *real dying
//! local process* makes this runtime emit, read off a real socket byte by byte.
//!
//! # Why the Erlang-level assertion is not enough
//!
//! Every other A.5 suite stops at the observable Erlang surface: a peer's
//! `erlang:monitor/2` on a local process eventually yields a `{'DOWN', Ref, process,
//! Pid, Reason}` message. That surface can show *that* a `DOWN` arrived, but **not**:
//!
//! * which of the two pids the control named **first**, and
//! * which opcode carried it.
//!
//! Both are real wire choices that a surface-level test cannot distinguish, and both
//! have a plausible-looking wrong answer:
//!
//! * `erts/emulator/beam/dist.c:1497-1522` (`erts_dsig_send_m_exit`) writes
//!   `TUPLE5(DOP_MONITOR_P_EXIT, Watched, Watcher, Ref, Reason)` — **watched (the local,
//!   dying process) first**, which is the *reverse* of the monitor family's own order
//!   (`{DOP_MONITOR_P, Watcher, Watched, Ref}`, `dist.c:2341`). A decoder that swapped the
//!   two would still hand a live peer a perfectly plausible DOWN — it would simply watch
//!   the wrong process, and only a peer that compares the pid it is told about against the
//!   one it monitors would ever notice.
//! * the monitor family has a near-twin opcode, `DOP_EXIT`/`DOP_EXIT2` (exit signals). An
//!   encoder that emitted the DOWN as a plain exit would again kill/wake the right process
//!   with the right reason; the receiving `erl` would just never complete the monitor.
//!
//! So this test asserts the **bytes**: opcode `21` (`DOP_MONITOR_P_EXIT`), the argument
//! count, the *identity* of argument 0 (this node's own pid for the dying process) and
//! argument 1 (the peer's pid, exactly as the peer sent it), the reference element, and the
//! reason element.
//!
//! # Hermetic
//!
//! No `erl`, no `erlc`, no `epmd`, no child VM: an in-process [`Runtime`] configured as a
//! `-sname` node (so it binds a real distribution listener) plus a test-side peer built from
//! [`beam_dist::ConnectionDriver::client`] over a real `TcpStream` to that listener. The
//! whole monitor round trip is distribution-driven: the peer sends `DOP_MONITOR_P`, the peer
//! kills the watched process with a real `DOP_EXIT2`, and the runtime's own termination path
//! turns that into the `DOP_MONITOR_P_EXIT` this test reads back. Nothing here needs the
//! toolchain, so the test never skips.
//!
//! # Running it
//!
//! It is `#[ignore]`d only because it is a wire-level integration case (it binds a socket
//! and drives a scheduler pass by hand), not because it needs anything installed:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_rt --test dist_two_vm_wire -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::{external_pid_parts, external_ref_parts, make_external_pid, make_external_ref};
use beam_dist::flags::{
    Dop, DFLAG_HANDSHAKE_23, DFLAG_UNLINK_ID, DOP_EXIT, DOP_EXIT2, DOP_MONITOR_P_EXIT,
};
use beam_dist::{ConnectionDriver, DistFlags, DriverState, StepOutcome};
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::prelude::make_atom;
use beam_term::AtomTable;

/// The creation the **test-side peer** claims for itself. Any value works; it must be
/// distinct from the runtime's own so a pid of the peer can never be mistaken for a local
/// one.
const PEER_CREATION: u32 = 0x00aa_55aa;

/// How long one handshake/drain loop may take before the test gives up (the loop itself is
/// bounded by iterations too — this is the backstop, never the schedule).
const WIRE_DEADLINE: Duration = Duration::from_secs(30);

/// A cookie only this test knows.
const COOKIE: &str = "wire_cookie";

/// A name unique to this test **process**, so two concurrent runs cannot collide.
fn unique(tag: &str) -> String {
    format!("wire{tag}{}", std::process::id())
}

/// Starts a `-sname` runtime through the **CLI parser**, so the configuration is exactly
/// what the command line produces (`tests/dist.rs`'s shape).
fn node_runtime(name: &str) -> Runtime {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", COOKIE, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).expect("the node switches parse");
    Runtime::new(config).expect("the runtime starts (the listener binds)")
}

/// The test-side half of one link: the peer's driver, its socket, its own atom table (the
/// namespace its outbound terms are encoded against) and the decode arena its inbound
/// messages land on.
struct Peer {
    /// The peer's own `node@host` name — the name its `'N'` handshake carries.
    name: String,
    driver: ConnectionDriver,
    stream: TcpStream,
    atoms: AtomTable,
    heap: Heap,
}

impl Peer {
    /// Dials the runtime's listener as `peer_name` and completes the `'N'` handshake.
    ///
    /// Both sides are driven by hand — the runtime by one scheduler pass, the peer by
    /// [`ConnectionDriver::step`] over the bytes it reads — because neither side may block
    /// the other.
    fn handshake(rt: &mut Runtime, port: u16, peer_name: &str) -> Peer {
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("dial the runtime's listener");
        stream
            .set_read_timeout(Some(Duration::from_millis(25)))
            .expect("a read timeout on the peer socket");
        stream.set_nodelay(true).expect("TCP_NODELAY");
        let mut peer = Peer {
            name: peer_name.to_string(),
            driver: ConnectionDriver::client(peer_name.as_bytes(), COOKIE.as_bytes(), PEER_CREATION),
            stream,
            atoms: AtomTable::new(),
            heap: Heap::new(1 << 12).expect("the decode arena"),
        };
        let deadline = Instant::now() + WIRE_DEADLINE;
        for _ in 0..600 {
            peer.drive(rt);
            if matches!(peer.driver.state(), DriverState::Established)
                && rt.dist_node().is_some_and(|node| node.has_established_link())
            {
                return peer;
            }
            assert!(Instant::now() < deadline, "the handshake never completed");
        }
        panic!(
            "the handshake never completed (peer state {:?})",
            peer.driver.state()
        );
    }

    /// One turn of the loop: let the runtime make progress, then move whatever bytes are in
    /// flight in both directions and decode whatever the peer has received.
    fn drive(&mut self, rt: &mut Runtime) {
        rt.run_one_slice().expect("a scheduler pass");
        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => self.driver.feed_eof(),
            Ok(n) => self.driver.feed(&buf[..n]),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(err) => panic!("peer read: {err}"),
        }
        loop {
            match self.driver.step() {
                Ok(StepOutcome::WouldBlock) => break,
                Ok(_) => {}
                Err(err) => panic!("peer step: {err}"),
            }
        }
        if self.driver.has_output() {
            let bytes = self.driver.take_output();
            self.stream.write_all(&bytes).expect("peer write");
            self.stream.flush().expect("peer flush");
        }
    }

    /// The next decoded control message, if the peer has one buffered.
    fn next_control(&mut self) -> Option<beam_dist::ControlMessage> {
        self.driver
            .next_message(&mut self.heap, &mut self.atoms)
            .expect("the peer decodes its inbound control messages")
    }

    /// Drives the runtime until it writes a control the peer can decode, bounded by
    /// `WIRE_DEADLINE`. Panics with the driver state when nothing arrives.
    fn await_control(&mut self, rt: &mut Runtime, what: &str) -> beam_dist::ControlMessage {
        let deadline = Instant::now() + WIRE_DEADLINE;
        loop {
            self.drive(rt);
            if let Some(message) = self.next_control() {
                return message;
            }
            assert!(
                Instant::now() < deadline,
                "the runtime never emitted {what} (peer state {:?}, runtime link error {:?})",
                self.driver.state(),
                rt.dist_node().and_then(|node| node.last_link_error().map(str::to_string)),
            );
        }
    }

    /// An external pid for the peer's own node, built on the peer's throwaway arena.
    fn peer_pid(&mut self, number: u32, serial: u32) -> beam_term::Eterm {
        let node = make_atom(self.atoms.intern(&self.name));
        make_external_pid(&mut self.heap, node, number, serial, PEER_CREATION)
            .expect("an external pid for the peer")
    }

}

/// The runtime's own advertised node name and creation, read from the live node.
fn runtime_identity(rt: &Runtime) -> (String, u32) {
    let node = rt.dist_node().expect("a `-sname` runtime is a DistNode");
    (node.node_name().to_string(), node.creation())
}

/// **Contract A.5, wire level.** A local process this node holds is watched by a peer over
/// a real link; the peer kills it with a real `DOP_EXIT2`; the runtime's own termination
/// path emits the `DOP_MONITOR_P_EXIT` and this test asserts the bytes.
///
/// The three things asserted here are exactly the three the Erlang surface cannot show:
///
/// 1. **opcode `21`** (`DOP_MONITOR_P_EXIT`) with the C's argument count — 4 control
///    arguments, i.e. the 5-element wire tuple of `dist.c:1497-1522`;
/// 2. **the watched process comes first**: argument 0 is a *local* pid of **this** node
///    carrying the **dying process's own** number and serial, and argument 1 is the
///    **peer's** pid exactly as the peer sent it in its `DOP_MONITOR_P` — the reverse of the
///    monitor family's order;
/// 3. the **negotiated `DFLAG` set** the two sides actually agreed, which must contain the
///    mandatory set a real OTP peer requires (`DFLAG_HANDSHAKE_23`/`DFLAG_UNLINK_ID`).
#[test]
#[ignore = "wire-level A.5 integration case (binds a socket, drives the scheduler by hand)"]
fn the_outbound_monitor_down_control_is_opcode_21_with_the_watched_process_first() {
    let name = unique("n");
    let mut rt = node_runtime(&name);
    // The first pass binds nothing new but publishes the node (an absent `epmd` is
    // recorded, never raised — accepting needs no daemon).
    rt.run_one_slice().expect("a scheduler pass");
    let port = rt.dist_node().expect("a DistNode").port();
    assert!(port > 0, "the listener bound a real port");

    let (local_node_name, local_creation) = runtime_identity(&rt);
    let peer_name = format!("{}@localhost", unique("p"));
    let mut peer = Peer::handshake(&mut rt, port, &peer_name);

    // ---------------------------------------------------------------------------------
    // (3) the negotiated flag set — asserted first, because the rest depends on it.
    // ---------------------------------------------------------------------------------
    let runtime_flags = rt
        .dist_node()
        .expect("a DistNode")
        .advertised_flags();
    let server_flags = peer.driver.server_flags();
    let negotiated = peer.driver.negotiated();
    assert_eq!(
        server_flags, runtime_flags,
        "the flags the peer read off the `'N'` challenge (0x{:08x}) are the ones the runtime \
         advertises (0x{:08x})",
        server_flags.bits(),
        runtime_flags.bits()
    );
    for (bit, label) in [
        (DFLAG_HANDSHAKE_23, "DFLAG_HANDSHAKE_23"),
        (DFLAG_UNLINK_ID, "DFLAG_UNLINK_ID"),
    ] {
        assert!(
            negotiated.contains(DistFlags::from_bits(bit)),
            "the link did not negotiate {label} (0x{bit:08x}); negotiated = 0x{:08x}",
            negotiated.bits()
        );
    }
    eprintln!(
        "negotiated DFLAG set = 0x{:08x} (runtime advertises 0x{:08x}; peer's own 0x{:08x})",
        negotiated.bits(),
        runtime_flags.bits(),
        peer.driver.client_flags().bits()
    );

    // ---------------------------------------------------------------------------------
    // The watched process: a real process of this node, parked in `receive`.
    // ---------------------------------------------------------------------------------
    rt.load_module(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/recv_after.beam"
    ))
    .expect("the fixture loads");
    let watched = rt
        .spawn_process("recv_after", "wait_forever", Vec::new())
        .expect("the watched process is created");
    // One pass so it enters its `receive` and parks: a live, killable local process.
    rt.run_one_slice().expect("a scheduler pass");
    assert!(
        rt.processes().contains(watched),
        "the watched process is live before the peer monitors it"
    );

    // The terms the peer puts on the wire, in **its** namespace.
    let watcher_number = 7;
    let watcher_serial = 0;
    let watcher = peer.peer_pid(watcher_number, watcher_serial);
    let watched_node = make_atom(peer.atoms.intern(&local_node_name));
    let watched_term = make_external_pid(
        &mut peer.heap,
        watched_node,
        watched.number() as u32,
        watched.serial() as u32,
        local_creation,
    )
    .expect("an external pid naming the runtime's own node");
    let reference_ids = [0u32, 0x1234_5678, 0x9abc_def0];
    let reference = make_external_ref(
        &mut peer.heap,
        make_atom(peer.atoms.intern(&peer_name)),
        &reference_ids,
        PEER_CREATION,
    )
    .expect("the peer's own reference");

    // ---------------------------------------------------------------------------------
    // The peer installs the monitor: `{DOP_MONITOR_P, Watcher, Watched, Ref}`.
    // ---------------------------------------------------------------------------------
    peer.driver
        .send_control(Dop::MonitorP, &[watcher, watched_term, reference], &peer.atoms)
        .expect("the peer encodes DOP_MONITOR_P");
    for _ in 0..40 {
        peer.drive(&mut rt);
        if !rt.processes().remote_watchers(watched).is_empty() {
            break;
        }
    }
    let recorded = rt.processes().remote_watchers(watched);
    assert_eq!(
        recorded.len(),
        1,
        "the runtime recorded exactly one remote watcher for the watched process"
    );

    // ---------------------------------------------------------------------------------
    // The peer kills it over the link: `{DOP_EXIT2, From, To, killed}` — a real,
    // distribution-driven termination, so the DOWN below is the runtime's own.
    // ---------------------------------------------------------------------------------
    let killed = make_atom(peer.atoms.intern("killed"));
    peer.driver
        .send_control(Dop::Exit2, &[watcher, watched_term, killed], &peer.atoms)
        .expect("the peer encodes DOP_EXIT2");
    let deadline = Instant::now() + WIRE_DEADLINE;
    while rt.processes().contains(watched) {
        assert!(
            Instant::now() < deadline,
            "the remote exit signal never terminated the watched process"
        );
        peer.drive(&mut rt);
    }

    // ---------------------------------------------------------------------------------
    // (1) + (2): the runtime's own DOP_MONITOR_P_EXIT, read off the socket.
    // ---------------------------------------------------------------------------------
    let message = peer.await_control(&mut rt, "DOP_MONITOR_P_EXIT");
    assert_eq!(
        DOP_MONITOR_P_EXIT, 21,
        "`DOP_MONITOR_P_EXIT` is opcode 21 (`erts/emulator/beam/dist.h`); the decoded `Dop` \
         below is what that very byte maps to"
    );
    assert_eq!(
        message.dop,
        Dop::MonitorPExit,
        "the DOWN control carries opcode {DOP_MONITOR_P_EXIT} (`DOP_MONITOR_P_EXIT`), not an \
         exit signal (`DOP_EXIT` = {DOP_EXIT}, `DOP_EXIT2` = {DOP_EXIT2})"
    );
    assert_eq!(
        message.terms.len(),
        4,
        "the wire tuple is `{{DOP_MONITOR_P_EXIT, Watched, Watcher, Ref, Reason}}`: four \
         arguments after the opcode"
    );

    // Argument 0 — the **watched** process, i.e. this node's own pid for the process that
    // just died. It must carry *this* node's name and creation, and the dying process's own
    // number/serial: the reverse of `DOP_MONITOR_P`'s order.
    let (node, number, serial, creation) =
        external_pid_parts(message.terms[0]).expect("argument 0 is an external pid");
    assert_eq!(
        node,
        make_atom(peer.atoms.intern(&local_node_name)),
        "argument 0 (the watched process) names **this** node"
    );
    assert_eq!(
        creation, local_creation,
        "…at this node's creation"
    );
    assert_eq!(
        u32::from(number),
        watched.number() as u32,
        "…and carries the dying process's own number"
    );
    assert_eq!(
        u32::from(serial),
        watched.serial() as u32,
        "…and its own serial"
    );

    // Argument 1 — the **watcher**, the peer's pid exactly as the peer sent it.
    let (node, number, serial, creation) =
        external_pid_parts(message.terms[1]).expect("argument 1 is an external pid");
    assert_eq!(
        node,
        make_atom(peer.atoms.intern(&peer_name)),
        "argument 1 (the watcher) is the **peer's** pid, not this node's — a swapped \
         encoder would have put the local pid here"
    );
    assert_eq!(creation, PEER_CREATION, "…at the peer's creation");
    assert_eq!(number, watcher_number, "…and the watcher's own number");
    assert_eq!(serial, watcher_serial, "…and the watcher's own serial");

    // Argument 2 — the reference the peer minted, echoed **verbatim** so the peer's own
    // `{'DOWN', Ref, …}` matches the monitor it installed.
    let (node, ids, creation) =
        external_ref_parts(message.terms[2]).expect("argument 2 is an external reference");
    assert_eq!(
        node,
        make_atom(peer.atoms.intern(&peer_name)),
        "the reference keeps the peer's node"
    );
    assert_eq!(creation, PEER_CREATION, "…and the peer's creation");
    assert_eq!(ids, reference_ids, "…and the peer's own id words");

    // Argument 3 — the reason the process actually died of.
    assert!(
        beam_term::compare::eq(message.terms[3], killed, &peer.atoms),
        "argument 3 is the reason the wire carried (`killed`), got {:?}",
        message.terms[3]
    );
}
