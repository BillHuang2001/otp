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
//! **Accepted-socket option inheritance** (contract A.4) — the runtime half of the C's
//! `tcp_inet_copy` (`erts/emulator/drivers/common/inet_drv.c:11834`), driven with **real loopback
//! TCP traffic**.
//!
//! # What the C does, and what this runtime has to do instead
//!
//! The C's socket drivers create the accepted port themselves: `tcp_inet_copy` copies the
//! listening descriptor's option block field by field into the accepted one
//! (`inet_drv.c:11810-11827`) and `async_ok_port` hands the Erlang side `{ok, Port2}`. A
//! [`beam_io::Driver`] here cannot create a port, so the accepted descriptor rides
//! [`InetAsyncResult::Accepted`] up to the runtime, which registers the port
//! ([`PortRegistry::adopt_accepted`]) and then **replays** the listener's inherited block onto it
//! ([`PortRegistry::inherit_inet_options`]) through the driver's own `setopts` surface.
//!
//! So this suite asserts the *observable* half of that contract on a port the runtime really
//! adopted from a real `accept(2)`: the accepted socket's driver state must agree with its
//! listener's for exactly the set `tcp_inet_copy` copies, and must **not** inherit
//! `{active, …}` — the C does not copy `active`/`active_count`, the Erlang side re-applies it
//! (`prim_inet:accept_opts/3`, `erts/preloaded/src/prim_inet.erl:502-517`), and the driver's
//! `INET_LOPT_ACTIVE` arm *adds* its operand to the live counter, so a replay would double-arm it.
//!
//! # How the evidence is read
//!
//! Every assertion is read back **through the driver** with a `getopts` control op on the accepted
//! port, i.e. the state a `gen_tcp`/`inet` caller would see, never a Rust-side copy of what the
//! replay claimed to do:
//!
//! | option | listener's `getopts` | accepted port's `getopts` | the C |
//! |---|---|---|---|
//! | `{packet, 2}` (23) | `[1,23,0,0,0,2]` | `[1,23,0,0,0,2]` | copied (`htype`) |
//! | `{packet_size, 4096}` (32) | `[1,32,0,0,16,0]` | `[1,32,0,0,16,0]` | copied (`psize`) |
//! | `{header, 8}` (21) | `[1,21,0,0,0,8]` | `[1,21,0,0,0,8]` | copied (`hsz`) |
//! | `{mode, list}` (24) | `[1,24,0,0,0,0]` | `[1,24,0,0,0,0]` | copied (`mode`) |
//! | `{active, 3}` (22) | `[1,22,0,0,0,3,0,3]` | `[1,22,0,0,0,0]` | **not** copied |
//!
//! The last row is the point of the whole exercise: the listener is in the multi (counter) form
//! and the accepted socket is left at the driver's passive default, i.e. the counter was **not**
//! doubled. [`PortRegistry::data_mode`](super::super::PortRegistry) must also answer
//! [`StreamType::List`] for the accepted port — the listener's *data* mode, which
//! [`PortMeta::stream`](super::super::PortMeta) cannot carry because `prim_inet:open/5` always
//! opens the driver port `[binary]`.
//!
//! One further row proves the replayed block is live on the wire rather than only in the driver's
//! option store: the named `send` verb on the accepted port emits the `{packet, 2}` length header
//! before the payload (`inet_drv.c:13745-13769`), so the peer reads `<<0,2,"hi">>` — the framed
//! write an un-inherited socket would not produce.
//!
//! # The negative rows
//!
//! * A `setopts` the driver **rejects** (`tcp_inet_copy` has no failure mode — a replay does) must
//!   leave the mirror at its pre-call state, so a value the driver refused can never be replayed
//!   onto a later accepted socket. Both rejections are covered: the driver's own `[0]einval` reply
//!   (an unserved option code) and its typed `badarg` (a truncated `{active, N}` record).
//! * A record the driver rejects **during the replay** is not ignored: the freshly adopted port is
//!   closed (which releases the descriptor — nothing leaks) and the accept completes as
//!   `{error, einval}` — the errno the driver itself answers for a bad option value. The record
//!   that trips this is written straight into the listener's mirror, because no real listener can
//!   hold one its own driver refused: it exercises the replay rule, which is otherwise unreachable.
//!
//! # The `note_inet_setopts` hook
//!
//! [`PortRegistry::port_control`] is where a real `setopts` becomes part of the mirror
//! (`inet_opts`' module docs). That one-line call is **not wired yet** in `src/port.rs` (it is
//! owned by another agent), so every `setopts` this suite issues is followed by an explicit
//! [`PortRegistry::note_inet_setopts`] with the reply's own success tag — the same call, from the
//! same reply, that the hook makes. When the hook lands these explicit calls become no-ops (they
//! record the same argument a second time only if the hook already did, which the suite's
//! `records()` assertions would surface immediately).
//!
//! # The oracle
//!
//! [`the_oracle_copies_the_same_option_set_into_an_accepted_socket`] runs the equivalent sequence
//! under the system `erl` (`gen_tcp:listen` + a real loopback client + `gen_tcp:accept` +
//! `inet:getopts`) and pins the C's answers to the literals the rows above assert. A missing
//! toolchain, or a probe that cannot complete, is a **loud skip** — never a silent pass and never
//! a panic; every runtime assertion above stands on its own.
use std::cell::RefCell;
use std::io::Read;
use std::net::TcpStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_bifs::PortHook;
use beam_common::BeamError;
use beam_io::{CompletionEvent, InetAsyncResult, InetReason};
use beam_process::PortId;

use super::super::ops::inet_opts::{is_inherited_record, is_setopts_control, setopts_succeeded};
use super::super::tests::{fixture, skip_oracle_check, Fixture};
use super::super::{PortEvent, PortRegistry, StreamType};

// -----------------------------------------------------------------------------------------------
// The wire vocabulary (`inet_drv.c`'s option codes and request numbers)
// -----------------------------------------------------------------------------------------------

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the Erlang
/// side adds to every `inet` control request, which both `*_inet_ctl` entries subtract again.
const INET_CONTROL_MAGIC: u64 = 0x03f1_a300;
/// `INET_REQ_ACCEPT` (26) — the **multi-operation** accept, the only form that hands the accepted
/// descriptor to the runtime (`inet_drv.c:12194`). The *named* `accept` verb is a different
/// contract (it takes the connection onto the listening port), so the numeric form is used here.
const INET_REQ_ACCEPT: u64 = 26;
/// `INET_REQ_SETOPTS` (7) — the request the option mirror listens for.
const INET_REQ_SETOPTS: u64 = 7;
/// `INET_LOPT_HEADER` (21) — `desc->inet.hsz`, the packet header size.
const INET_LOPT_HEADER: u8 = 21;
/// `INET_LOPT_ACTIVE` (22) — `desc->inet.active` plus, in the multi form, a 16-bit counter.
const INET_LOPT_ACTIVE: u8 = 22;
/// `INET_LOPT_PACKET` (23) — `desc->inet.htype`, the C's `TCP_PB_*` code (`{packet,2}` ⇒ `2`).
const INET_LOPT_PACKET: u8 = 23;
/// `INET_LOPT_MODE` (24) — `desc->inet.mode` (`INET_MODE_LIST` `0`, `INET_MODE_BINARY` `1`).
const INET_LOPT_MODE: u8 = 24;
/// `INET_LOPT_PACKET_SIZE` (32) — `desc->inet.psize`, the largest frame the driver will assemble.
const INET_LOPT_PACKET_SIZE: u8 = 32;
/// `INET_LOPT_MSGQ_HIWTRMRK` (36) — a **copied** code whose set form the driver refuses below
/// `ERL_DRV_BUSY_MSGQ_LIM_MIN` (`1`), which is what makes it the replay-failure record below.
const INET_LOPT_MSGQ_HIWTRMRK: u8 = 36;
/// `INET_MULTI` (3) — the `{active, N}` form, whose record carries a trailing 16-bit counter.
const INET_MULTI: i32 = 3;
/// `INET_PASSIVE` (0) — the driver's default `active`, and what a **not** replayed counter leaves.
const INET_PASSIVE: i32 = 0;
/// `{mode, list}` — `INET_MODE_LIST` (`inet_drv.c:786`).
const INET_MODE_LIST: i32 = 0;
/// `TCP_PB_2` — the `{packet, 2}` code (`inet_drv.c:781`): a 2-byte big-endian length header.
const TCP_PB_2: i32 = 2;
/// `{packet_size, 4096}` — a limit the framed write below stays well inside.
const PACKET_SIZE: i32 = 4096;
/// `{header, 8}` — the C's `hsz`, copied to the accepted descriptor like everything else here.
const HEADER_SIZE: i32 = 8;
/// The accept's `Timeout(4)` operand: two seconds, so a probe can never expire mid-round
/// (`queued_accept_stream`'s own choice in the sibling `inet_matrix_tests`).
const ACCEPT_TIMEOUT_MS: i32 = 2_000;
/// How long the pump may take to adopt the accepted descriptor before the environment is declared
/// unable to run the case (a loud skip, never a silent pass).
const ACCEPT_BOUND: Duration = Duration::from_secs(10);
/// One pump round's slice — the port poll budget a zero-slice scheduler pass hands this layer.
const PUMP_SLICE: Duration = Duration::from_millis(50);

// -----------------------------------------------------------------------------------------------
// The wire helpers
// -----------------------------------------------------------------------------------------------

/// One `[opt, val_be32]` `setopts` record — the C's five-byte unit (`inet_drv.c:6908`).
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut bytes = vec![code];
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

/// One `{active, N}` record: `[22, INET_MULTI, counter_be16]` (`inet_drv.c:6977`).
fn active_counter_record(counter: i16) -> Vec<u8> {
    let mut bytes = record(INET_LOPT_ACTIVE, INET_MULTI);
    bytes.extend_from_slice(&counter.to_be_bytes());
    bytes
}

/// The `setopts` control op **as `prim_inet` sends it**: the decimal `c_uint` of
/// `INET_REQ_SETOPTS + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`.
fn setopts_op() -> String {
    let op = (INET_CONTROL_MAGIC + INET_REQ_SETOPTS).to_string();
    assert!(
        is_setopts_control(&op),
        "the numeric `setopts` opcode {op} must be the mirror's own request"
    );
    op
}

/// The numeric `accept` control op (`INET_REQ_ACCEPT`, 26).
fn accept_op() -> String {
    (INET_CONTROL_MAGIC + INET_REQ_ACCEPT).to_string()
}

/// Runs one control op and fails loudly with the driver's own error text.
fn control(reg: &mut PortRegistry, id: PortId, op: &str, arg: &[u8]) -> Vec<u8> {
    reg.port_control(id, op, arg)
        .unwrap_or_else(|error| panic!("the {op} control op on {id:?}: {error}"))
}

/// Runs a `setopts` op and mirrors the reply into the port's option table — the documented
/// stand-in for the (not yet wired, `src/port.rs`-owned) `note_inet_setopts` hook.
///
/// Returns the driver's reply, so a caller can assert on `[1]`/`[0]einval`.
fn setopts(reg: &mut PortRegistry, id: PortId, arg: &[u8]) -> Vec<u8> {
    let reply = control(reg, id, &setopts_op(), arg);
    let ok = setopts_succeeded(&reply);
    reg.note_inet_setopts(id, arg, ok);
    reply
}

/// The driver's reply to a `getopts` request for `code`, as raw bytes.
fn getopts(reg: &mut PortRegistry, id: PortId, code: u8) -> Vec<u8> {
    control(reg, id, "getopts", &[code])
}

/// The `setopts` argument the listener of every case is configured with: `{packet,2}`,
/// `{mode,list}`, `{packet_size,4096}`, `{header,8}` and `{active,3}` with a counter of `3`.
///
/// The order is the C's arrival order, which the mirror preserves and the replay re-sends.
fn listener_options() -> Vec<u8> {
    let mut arg = record(INET_LOPT_PACKET, TCP_PB_2);
    arg.extend_from_slice(&record(INET_LOPT_MODE, INET_MODE_LIST));
    arg.extend_from_slice(&record(INET_LOPT_PACKET_SIZE, PACKET_SIZE));
    arg.extend_from_slice(&record(INET_LOPT_HEADER, HEADER_SIZE));
    arg.extend_from_slice(&active_counter_record(3));
    arg
}

// -----------------------------------------------------------------------------------------------
// The fixture: a real listening `tcp_inet` port and a real client
// -----------------------------------------------------------------------------------------------

/// What the runtime made of one accepted descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adoption {
    /// The descriptor became a live `tcp_inet` port (the C's `{ok, Port2}`).
    Port(PortId),
    /// The replay was refused, so the port was closed and the operation answered `{error, Reason}`
    /// (the C's `send_async_error`).
    Rejected(InetReason),
}

/// A `tcp_inet` port opened through the runtime's own `{spawn_driver, "tcp_inet"}` path, bound to
/// a real loopback port and listening — the state `gen_tcp:listen/2` leaves the port in.
struct Listener {
    /// The runtime's port table (the one `RuntimePorts::open_port` registered the port in).
    registry: Rc<RefCell<PortRegistry>>,
    /// The fixture that owns the term vocabulary and the heap the `Name`/`Options` live on, so it
    /// must outlive the port (the registered `PortMeta` holds no copy of those terms).
    #[allow(dead_code)]
    fx: Fixture,
    /// The listening port.
    id: PortId,
    /// The kernel-assigned loopback port the accepted client connects to.
    port: u16,
}

impl Listener {
    /// Opens, binds and listens — or fails loudly naming the step.
    ///
    /// `None` when the environment cannot give this case a loopback socket
    /// ([`skip_no_loopback`] has then printed the banner): a sandbox that refuses
    /// `socket(2)`/`bind(2)` cannot host a real-traffic case, and a silent pass would be a lie.
    fn open() -> Option<Listener> {
        let mut fx = fixture();
        let name = {
            let driver = fx.string("tcp_inet");
            fx.name("spawn_driver", driver)
        };
        let options = {
            // `prim_inet:open/5` always opens the driver port `[binary]`; the *data* mode is a
            // driver option (`{mode, list}`) applied afterwards through `setopts`.
            let binary = fx.atom("binary");
            fx.list(&[binary])
        };
        let id = match fx.hook.open_port(name, options) {
            Ok(id) => id,
            Err(error) => {
                skip_no_loopback(&format!("open_port({{spawn_driver,\"tcp_inet\"}}) failed: {error}"));
                return None;
            }
        };
        let registry = Rc::clone(&fx.registry);
        let listener = Listener {
            registry,
            fx,
            id,
            port: 0,
        };
        Some(listener)
    }

    /// The port table, borrowed for one step.
    fn reg(&self) -> std::cell::RefMut<'_, PortRegistry> {
        self.registry
            .borrow_mut()
    }

    /// `open` → `bind` (loopback, kernel-assigned port) → `listen` — the C's three requests, with
    /// the same operands `prim_inet:open/5` + `gen_tcp:listen/2` send.
    fn bind_and_listen(&mut self) {
        let mut reg = self.reg();
        let opened = control(&mut reg, self.id, "open", &[1, 1, 0]);
        assert_eq!(opened, vec![1], "INET_REQ_OPEN on a stream/inet port");
        let bound = control(&mut reg, self.id, "bind", &[1, 0, 0, 127, 0, 0, 1]);
        assert_eq!(
            bound.first(),
            Some(&1),
            "INET_REQ_BIND answers `[1] ++ port`: {bound:?}"
        );
        assert_eq!(bound.len(), 3, "bind reports the ephemeral port: {bound:?}");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned a real port");
        let listened = control(&mut reg, self.id, "listen", &[0, 5]);
        assert_eq!(listened, vec![1], "INET_REQ_LISTEN with a backlog of 5");
        drop(reg);
        self.port = port;
    }

    /// Applies [`listener_options`] through the numeric `setopts` request and asserts the driver
    /// took every record.
    fn configure(&mut self) {
        let arg = listener_options();
        let mut reg = self.reg();
        let reply = setopts(&mut reg, self.id, &arg);
        assert_eq!(
            reply,
            vec![1],
            "the driver accepts {{packet,2}}, {{mode,list}}, {{packet_size,4096}}, {{header,8}} \
             and {{active,3}}"
        );
    }

    /// A real `std::net` client connected to the listening port, with a read timeout so a missing
    /// reply can never hang the suite.
    fn connect(&self) -> Option<TcpStream> {
        match TcpStream::connect(("127.0.0.1", self.port)) {
            Ok(client) => {
                let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
                // The client is only useful once the kernel has completed the handshake, which a
                // `connect(2)` on loopback has by the time it returns; a peer that is really
                // listening is what the accept below needs.
                Some(client)
            }
            Err(error) => {
                skip_no_loopback(&format!(
                    "TcpStream::connect(127.0.0.1:{}) failed: {error}",
                    self.port
                ));
                None
            }
        }
    }

    /// Queues the C's multi-operation `accept` and pumps until the runtime has adopted (or refused)
    /// the descriptor it took.
    ///
    /// `None` when no completion arrived inside [`ACCEPT_BOUND`] — the environment could not run
    /// the case ([`skip_no_loopback`] has printed the banner), never a silent pass.
    fn accept_one(&mut self) -> Option<Adoption> {
        let mut reg = self.reg();
        let reply = control(
            &mut reg,
            self.id,
            &accept_op(),
            &ACCEPT_TIMEOUT_MS.to_be_bytes(),
        );
        assert_eq!(
            reply.first(),
            Some(&1),
            "INET_REQ_ACCEPT answers `[1] ++ <<Id:16/big>>`: {reply:?}"
        );
        assert_eq!(reply.len(), 3, "the queued operation's id: {reply:?}");
        let deadline = Instant::now() + ACCEPT_BOUND;
        while Instant::now() < deadline {
            let events = match reg.pump(PUMP_SLICE) {
                Ok(events) => events,
                Err(error) => {
                    skip_no_loopback(&format!("the port round failed: {error}"));
                    return None;
                }
            };
            for event in events {
                let PortEvent::Completion { packet } = event else {
                    continue;
                };
                if let CompletionEvent::InetAsync { result, .. } = packet.event {
                    match result {
                        InetAsyncResult::Port(adopted) => return Some(Adoption::Port(adopted)),
                        InetAsyncResult::Error(reason) => {
                            return Some(Adoption::Rejected(reason))
                        }
                        _ => continue,
                    }
                }
            }
        }
        skip_no_loopback(&format!(
            "no accept completion within {} ms — the loopback accept did not complete here",
            ACCEPT_BOUND.as_millis()
        ));
        None
    }

    /// Closes both the listening port and `extra`, failing loudly if either teardown fails.
    fn close_all(&self, extra: Option<PortId>) {
        let mut reg = self.reg();
        if let Some(port) = extra {
            reg.close(port)
                .unwrap_or_else(|error| panic!("the accepted port {port:?} closes: {error}"));
        }
        reg.close(self.id)
            .unwrap_or_else(|error| panic!("the listening port {:?} closes: {error}", self.id));
    }
}

/// The loud skip banner for a case the **environment** cannot host: no loopback socket, or an
/// accept that never completed. Never a silent pass, never a panic.
fn skip_no_loopback(what: &str) {
    println!("================================================================");
    println!("SKIP: {what}");
    println!("      (this case needs real loopback TCP traffic and could not get it)");
    println!("================================================================");
}

// -----------------------------------------------------------------------------------------------
// Test 1 — the inherited block, read back through the driver, over real traffic
// -----------------------------------------------------------------------------------------------

/// An accepted socket inherits exactly the option block the C's `tcp_inet_copy` copies, and does
/// **not** inherit `{active, …}`.
///
/// The listener is told `{packet,2}`, `{mode,list}`, `{packet_size,4096}`, `{header,8}` and
/// `{active,3}` (counter 3) the way `prim_inet` tells it; a real `std::net` client connects over
/// loopback; the runtime adopts the descriptor the `accept` took and replays the block.
///
/// Every assertion is the oracle's own answer for the same sequence (cross-checked by
/// [`the_oracle_copies_the_same_option_set_into_an_accepted_socket`]).
#[test]
fn an_accepted_socket_inherits_the_listeners_option_block() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    listener.configure();

    // The listener's own state, before any accept: the driver really took all five records.
    {
        let mut reg = listener.reg();
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_PACKET),
            vec![1, INET_LOPT_PACKET, 0, 0, 0, 2],
            "the listener's `{{packet,2}}` (the C's TCP_PB_2 code)"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_PACKET_SIZE),
            vec![1, INET_LOPT_PACKET_SIZE, 0, 0, 16, 0],
            "the listener's `{{packet_size,4096}}`"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_HEADER),
            vec![1, INET_LOPT_HEADER, 0, 0, 0, HEADER_SIZE as u8],
            "the listener's `{{header,8}}`"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_MODE),
            vec![1, INET_LOPT_MODE, 0, 0, 0, 0],
            "the listener's `{{mode,list}}` (`INET_MODE_LIST` is 0)"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_ACTIVE),
            vec![1, INET_LOPT_ACTIVE, 0, 0, 0, INET_MULTI as u8, 0, 3],
            "the listener is in the multi (counter) form, with a live counter of 3"
        );
        // The mirror the replay reads from holds one entry per record, in arrival order.
        let mirrored = reg
            .inet_options(listener.id)
            .expect("a configured listener has an option mirror");
        let mirrored: Vec<Vec<u8>> = mirrored.records().to_vec();
        assert_eq!(
            mirrored.len(),
            5,
            "the mirror holds the five records the listener was told: {mirrored:?}"
        );
        assert_eq!(
            mirrored[0],
            record(INET_LOPT_PACKET, TCP_PB_2),
            "the first record is the listener's `{{packet,2}}`"
        );
        assert_eq!(
            mirrored[4],
            active_counter_record(3),
            "the last record is the listener's `{{active,3}}` with its counter"
        );
        assert!(
            !is_inherited_record(&mirrored[4]),
            "`{{active, N}}` is recorded but never replayed: the C does not copy `active`"
        );
        for record in &mirrored[..4] {
            assert!(
                is_inherited_record(record),
                "every other record is part of the C's copied block: {record:?}"
            );
        }
    }

    let Some(mut client) = listener.connect() else {
        return;
    };
    let Some(adopted) = listener.accept_one() else {
        return;
    };
    let Adoption::Port(accepted) = adopted else {
        panic!("a clean listener's accept must adopt the descriptor, got {adopted:?}");
    };
    {
        let reg = listener.reg();
        assert!(
            reg.is_live(accepted),
            "the adopted descriptor became a live port (the C's `{{ok, Port2}}`)"
        );
        assert_eq!(
            reg.len(),
            2,
            "exactly the listener and the accepted port are open"
        );
        assert_eq!(
            reg.data_mode(accepted, StreamType::Binary),
            StreamType::List,
            "the accepted port inherits the listener's *data* mode, not its `[binary]` stream option"
        );
        // Hook-agnostic mirror assertion: whatever the replay re-recorded must be part of the
        // copied block, and the active record must never appear (see the module docs on the
        // `note_inet_setopts` hook).
        let replayed: Vec<Vec<u8>> = reg
            .inet_options(accepted)
            .map(|options| options.records().to_vec())
            .unwrap_or_default();
        assert!(
            replayed.iter().all(|record| is_inherited_record(record)),
            "only the C's copied block may be re-recorded on the accepted port: {replayed:?}"
        );
        assert!(
            !replayed.contains(&active_counter_record(3)),
            "the `{{active,3}}` record must never be replayed: {replayed:?}"
        );
    }

    // The replayed effect, read back through the accepted port's own driver.
    {
        let mut reg = listener.reg();
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_PACKET),
            vec![1, INET_LOPT_PACKET, 0, 0, 0, 2],
            "the accepted socket's `{{packet,2}}` — copied, as `tcp_inet_copy` does"
        );
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_PACKET_SIZE),
            vec![1, INET_LOPT_PACKET_SIZE, 0, 0, 16, 0],
            "the accepted socket's `{{packet_size,4096}}`"
        );
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_HEADER),
            vec![1, INET_LOPT_HEADER, 0, 0, 0, HEADER_SIZE as u8],
            "the accepted socket's `{{header,8}}`"
        );
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_MODE),
            vec![1, INET_LOPT_MODE, 0, 0, 0, 0],
            "the accepted socket's `{{mode,list}}`"
        );
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_ACTIVE),
            vec![1, INET_LOPT_ACTIVE, 0, 0, 0, INET_PASSIVE as u8],
            "`{{active, N}}` is NOT replayed: the accepted socket stays passive, so the counter \
             cannot be doubled (the Erlang side re-applies it, `prim_inet:accept_opts/3`)"
        );
    }

    // …and the inherited `{packet,2}` is live on the wire: the named `send` verb frames the payload
    // with the 2-byte big-endian length header the C's `tcp_sendv` prepends.
    {
        let mut reg = listener.reg();
        let sent = control(&mut reg, accepted, "send", b"hi");
        assert_eq!(sent, vec![1], "the named `send` verb on a connected socket");
    }
    let mut framed = [0u8; 4];
    client
        .read_exact(&mut framed)
        .expect("the peer reads the framed write from the accepted socket");
    assert_eq!(
        framed,
        [0, 2, b'h', b'i'],
        "`{{packet,2}}` inherited from the listener frames the write: a 2-byte length header"
    );

    listener.close_all(Some(accepted));
}

// -----------------------------------------------------------------------------------------------
// Test 2 — a rejected `setopts` never enters the mirror
// -----------------------------------------------------------------------------------------------

/// A `setopts` the driver **rejects** leaves the mirror exactly as it was, so a value the driver
/// refused can never be replayed onto a later accepted socket.
///
/// Two rejections are covered, because the driver has two shapes for them:
///
/// * an **unserved option code** — the C's `default: RETURN_ERROR()` (`inet_drv.c:9719`), which is a
///   `[0]einval` **reply** (the operation itself succeeded, its tag says the record was refused);
/// * a **truncated `{active, N}`** record — a multi form without its trailing counter, which this
///   floor reports as the typed `badarg` instead of reading past the payload.
#[test]
fn a_rejected_setopts_never_enters_the_option_mirror() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    listener.configure();
    let before = {
        let reg = listener.reg();
        reg.inet_options(listener.id)
            .cloned()
            .expect("the configured listener's mirror")
    };

    {
        let mut reg = listener.reg();
        // (a) An unserved code: the driver's own `[0]einval` reply, not an error.
        let unserved = record(200, 1);
        let reply = control(&mut reg, listener.id, &setopts_op(), &unserved);
        assert_eq!(
            reply,
            b"\x00einval".to_vec(),
            "the C's `ctl_xerror(\"einval\")` reply for an option code the driver does not serve"
        );
        assert!(!setopts_succeeded(&reply), "a `[0]` tag is not INET_REP_OK");
        assert!(
            !reg.note_inet_setopts(listener.id, &unserved, setopts_succeeded(&reply)),
            "a rejected record is never stored"
        );
        assert_eq!(
            reg.inet_options(listener.id),
            Some(&before),
            "the mirror is unchanged by a rejected `setopts`"
        );

        // (b) A truncated `{active, N}`: the driver cannot read the counter and refuses the call.
        let truncated = record(INET_LOPT_ACTIVE, INET_MULTI);
        assert!(
            matches!(
                reg.port_control(listener.id, &setopts_op(), &truncated),
                Err(BeamError::BadArg)
            ),
            "a multi `{{active, N}}` record without its counter is the driver's typed `badarg`"
        );
        assert!(
            !reg.note_inet_setopts(listener.id, &truncated, false),
            "a refused call is never stored either"
        );
        assert_eq!(
            reg.inet_options(listener.id),
            Some(&before),
            "the mirror is unchanged by the refused call"
        );
    }

    listener.close_all(None);
}

// -----------------------------------------------------------------------------------------------
// Test 3 — a record the driver rejects *during* the replay fails the whole accept
// -----------------------------------------------------------------------------------------------

/// A replay record the driver rejects is not ignored: the freshly adopted port is **closed** (its
/// descriptor released with it, so nothing leaks) and the accept operation completes as
/// `{error, einval}` — the errno the driver itself answers for a bad option value.
///
/// The record is written straight into the listener's mirror with a claimed success, because no
/// real listener can hold one its own driver refused — that is exactly the state the rule exists
/// for, and it is unreachable through `setopts`.
#[test]
fn a_replay_the_driver_rejects_closes_the_new_port_and_fails_the_accept() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    listener.configure();
    let poisoned = record(INET_LOPT_MSGQ_HIWTRMRK, 0);
    assert!(
        is_inherited_record(&poisoned),
        "the poisoning record must be part of the copied block, or the replay would skip it"
    );
    assert!(
        {
            // The driver's own refusal of the record, proved on the listener first: below
            // `ERL_DRV_BUSY_MSGQ_LIM_MIN` (1) the whole reply is `[0]einval`.
            let mut reg = listener.reg();
            let reply = control(&mut reg, listener.id, &setopts_op(), &poisoned);
            reply == b"\x00einval".to_vec()
        },
        "the driver refuses `{{high_msgq_watermark,0}}`, which is what makes the replay fail"
    );
    {
        let mut reg = listener.reg();
        assert!(
            reg.note_inet_setopts(listener.id, &poisoned, true),
            "the poisoning record is mirrored with a claimed success"
        );
    }

    let Some(_client) = listener.connect() else {
        return;
    };
    let Some(adopted) = listener.accept_one() else {
        return;
    };
    let Adoption::Rejected(reason) = adopted else {
        panic!("a poisoned replay must not adopt a port, got {adopted:?}");
    };
    assert_eq!(
        reason.atom_name(),
        "einval",
        "a rejected replay completes the accept as `{{error, einval}}`, the driver's own errno"
    );
    {
        let reg = listener.reg();
        assert_eq!(
            reg.len(),
            1,
            "the half-configured port was closed, so only the listener is left (nothing leaked)"
        );
        assert!(
            reg.is_live(listener.id),
            "the listening port itself survives a failed accept"
        );
    }

    listener.close_all(None);
}

// -----------------------------------------------------------------------------------------------
// Test 4 — the oracle
// -----------------------------------------------------------------------------------------------

/// The system `erl` copies the same option set into an accepted socket, so the literals
/// [`an_accepted_socket_inherits_the_listeners_option_block`] asserts are the C's own answers.
///
/// `gen_tcp:listen/2` applies the options to the listening descriptor, `gen_tcp:accept/1` makes the
/// new port through `tcp_inet_copy` + `prim_inet:accept_opts/3`, and `inet:getopts/2` reads the
/// accepted socket back — `{packet,2}`, `{packet_size,4096}`, `{header,8}` and `{mode,list}` are
/// `tcp_inet_copy`'s, which is precisely what this suite replays.
///
/// `{active, …}` is deliberately **not** compared on the accepted socket: the oracle's answer there
/// is `prim_inet:accept_opts/3`'s re-application from the Erlang side, not the driver's copy, and
/// this runtime has no `prim_inet` in the loop (the C's own `tcp_inet_copy` copies no `active`
/// either — which is the behaviour the accepted **port** in this suite exhibits).
#[test]
fn the_oracle_copies_the_same_option_set_into_an_accepted_socket() {
    let expr = "begin \
         {ok,L} = gen_tcp:listen(0,[list,{ip,{127,0,0,1}},{packet,2},{packet_size,4096},\
         {header,8}]), \
         {ok,{_,P}} = inet:sockname(L), \
         Self = self(), \
         _ = spawn(fun() -> {ok,_S} = gen_tcp:connect({127,0,0,1},P,[list]), \
         Self ! connected, receive after 3000 -> ok end end), \
         receive connected -> ok after 3000 -> exit(no_client) end, \
         {ok,A} = gen_tcp:accept(L), \
         {ok,O} = inet:getopts(A,[packet,packet_size,header,mode]), \
         io:format(\"~w\",[O]), gen_tcp:close(A), gen_tcp:close(L) end";
    let Some(output) = super::super::tests::erl_eval(expr) else {
        skip_oracle_check("accepted-socket option inheritance (tcp_inet_copy)");
        return;
    };
    assert_eq!(
        output, "[{packet,2},{packet_size,4096},{header,8},{mode,list}]",
        "the oracle's accepted socket carries exactly the block this runtime replays"
    );
}
