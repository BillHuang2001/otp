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

//! The peer-close channel's suites.
//!
//! Some cases are pure: the `{active, …}`/`{exit_on_close, …}` mirror reads, the once-only latch
//! with its recycle rule, and the driver-kind gate. The rest drive **real loopback traffic**
//! through a real `{spawn_driver, "tcp_inet"}` / `{spawn_driver, "udp_inet"}` port opened by the
//! runtime's own `open_port/2` hook, put bytes and/or a close through it, and read the latch back
//! through [`PortRegistry::drain_peer_closes`] — the two halves of the invariant that matter most
//! (invariant 1: once, and only on a socket that is active **now**; invariant 6: never for a
//! datagram port).
//!
//! # Loud skips, never a silent pass
//!
//! A sandbox that refuses `socket(2)`/`bind(2)` cannot host a real-traffic case: [`skip_no_loopback`]
//! prints a banner and the case returns. A missing `erl` skips only the oracle cross-check, with
//! [`skip_oracle_check`]'s banner. No case depends on another having run.
//!
//! # The oracle these cases reproduce (OTP 29.1.1 / erts-17.1, measured)
//!
//! * `tcp_recv_closed` (`erts/emulator/drivers/common/inet_drv.c:12783-12845`): an **active**
//!   socket sends one `{tcp_closed, S}` (2-tuple) and dies when `exit_on_close` (the default);
//!   a **passive** one sends nothing (`:12819-12832`).
//! * `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1552-1560`): the `{active, …}` grant is cleared **at the
//!   spend**, so a socket whose `{active, once}` grant is gone — and one whose `{active, N}`
//!   counter reached zero — is passive for that test, and its close is silent exactly like a
//!   never-armed socket's. `packet_passive_message` fires only for the `INET_MULTI` arm, never for
//!   `INET_ONCE`.
//! * `tcp_closed_message`'s `TCP_ADDF_CLOSE_SENT` guard (`:4064-4071`): at most once per
//!   descriptor, however often the driver observes the EOF.
//! * `inet_set_opts`'s `INET_LOPT_ACTIVE` arm (`:6968-7016`): a passive descriptor already closed
//!   sends the message **at that moment** when an active mode is armed — the sticky re-fire.
//! * `rg 'udp_closed' erts/` has **zero** hits: a `udp_inet` port has no such message.

use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_bifs::PortHook;
use beam_engine::terms;
use beam_io::{ActiveInputEvent, ActiveMode};
use beam_process::{PortId, ProcessId, ProcessTable, SignalAtoms};
use beam_term::{AtomTable, Eterm};

use super::{PeerCloseKind, PeerCloseReport};
use crate::config::RuntimeConfig;
use crate::port::ops::active_input::ActiveInputPacket;
use crate::port::ops::inet_opts::{is_setopts_control, setopts_succeeded};
use crate::port::tests::{erl_eval, fixture, skip_oracle_check, Fixture};
use crate::port::{PortEvent, RuntimeContext, RuntimePorts};
use crate::Runtime;

// -----------------------------------------------------------------------------------------------
// The wire vocabulary (the option codes and request numbers the C's `inet` driver parses)
// -----------------------------------------------------------------------------------------------

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the Erlang
/// side adds to every `inet` control request.
const INET_CONTROL_MAGIC: u64 = 0x03f1_a300;
/// `INET_REQ_SETOPTS` (7) — the request `prim_inet:setopts/2` sends for a record list.
const INET_REQ_SETOPTS: u64 = 7;
/// `TCP_REQ_RECV` (42) — the passive read `prim_inet:recv/3` sends (`inet_drv.c:12265-12325`),
/// whose enqueue is the C's `sock_select(INETP(desc), (FD_READ|FD_CLOSE), 1)` (`:12315`): the one
/// thing that arms a socket's read side out of a spent `{active, …}` grant's window.
const TCP_REQ_RECV: u64 = 42;
/// `INET_REQ_OPEN`'s socket family tag: `INET_AF_INET` (1).
const INET_AF_INET: u8 = 1;
/// `SOCK_STREAM` (1) — the `INET_REQ_OPEN` operand of a `tcp_inet` socket.
const INET_SOCK_STREAM: u8 = 1;
/// `SOCK_DGRAM` (2) — the `INET_REQ_OPEN` operand of a `udp_inet` socket.
const INET_SOCK_DGRAM: u8 = 2;
/// The C's "default protocol for this socket type" (0).
const INET_PROTO_DEFAULT: u8 = 0;
/// `INET_LOPT_ACTIVE` (22) — `{active, …}`, whose operand is the C's `desc->inet.active`.
const INET_LOPT_ACTIVE: u8 = 22;
/// `INET_LOPT_EXITONCLOSE` (26) — `{exit_on_close, …}`, whose operand is the C's `desc->inet.exitf`.
const INET_LOPT_EXITONCLOSE: u8 = 26;
/// `INET_ACTIVE` (1) — `{active, true}`.
const INET_ACTIVE: i32 = 1;
/// `INET_ONCE` (2) — `{active, once}`.
const INET_ONCE: i32 = 2;
/// `INET_MULTI` (3) — `{active, N}`, whose record carries a trailing counter (`inet_drv.c:786-789`).
const INET_MULTI: i32 = 3;
/// `INET_PASSIVE` (0) — `{active, false}`, the driver's default.
const INET_PASSIVE: i32 = 0;/// How long a case waits for a report it **expects**: a fan-out bound, never a sleep.
const DELIVERY_BOUND: Duration = Duration::from_secs(5);
/// How long a case polls to prove nothing is reported — six `PORT_POLL_SLICE` rounds, each of
/// which blocks on a real `epoll_wait`, so a report that exists has every chance to arrive.
const QUIET_WINDOW: Duration = Duration::from_millis(300);
/// One pump round's slice.
const PUMP_SLICE: Duration = Duration::from_millis(50);

/// One `[opt, val_be32]` `setopts` record — the C's five-byte unit (`inet_drv.c:6908`).
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut bytes = vec![code];
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

/// One `{active, N}` record: `[INET_LOPT_ACTIVE, INET_MULTI, counter_be16]` — the C's trailing
/// 2-byte big-endian counter, which `inet_set_opts` accumulates into `desc->inet.active_count`
/// (`inet_drv.c:6975-6987`).
fn multi_record(code: u8, value: i32, count: i16) -> Vec<u8> {
    let mut bytes = record(code, value);
    bytes.extend_from_slice(&count.to_be_bytes());
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

/// The C's packed IPv4 address record `[INET_AF_INET, port_hi, port_lo, 127, 0, 0, 1]`.
fn loopback_address(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_INET, hi, lo, 127, 0, 0, 1]
}

/// The loud skip banner for a case the **environment** cannot host.
fn skip_no_loopback(what: &str) {
    println!("================================================================");
    println!("SKIP: {what}");
    println!("      (this case needs real loopback traffic and could not get it)");
    println!("================================================================");
}

// -----------------------------------------------------------------------------------------------
// A registry without any port — the pure mirror/latch cases
// -----------------------------------------------------------------------------------------------

/// A bare registry, for the cases that only feed the option mirror or the latch.
fn bare_registry() -> crate::port::PortRegistry {
    crate::port::PortRegistry::new().expect("an empty port registry")
}

#[test]
fn the_active_mode_mirror_is_the_last_accepted_record_and_passive_when_unknown() {
    let mut registry = bare_registry();
    let id = PortId::new(1);
    // No `{active, …}` record at all ⇒ the C's `inet_start` default, `INET_PASSIVE`
    // (`inet_drv.c:10932`) — never an invented "active".
    assert!(
        !registry.inet_active(id),
        "a port with no `{{active,…}}` record is passive, as `inet_start` leaves it"
    );
    // `{active, false}` records the passive value.
    assert!(registry.note_inet_setopts(id, &record(INET_LOPT_ACTIVE, INET_PASSIVE), true));
    assert!(!registry.inet_active(id), "`{{active,false}}` is passive");
    // … and a later `{active, once}` is the one that counts (last record wins).
    assert!(registry.note_inet_setopts(id, &record(INET_LOPT_ACTIVE, INET_ONCE), true));
    assert!(registry.inet_active(id), "`{{active,once}}` is active");
    assert!(registry.note_inet_setopts(id, &record(INET_LOPT_ACTIVE, INET_ACTIVE), true));
    assert!(
        registry.inet_active(id),
        "`{{active,true}}` is active, and it is the last record"
    );
    // A **rejected** record is never recorded (`note_inet_setopts` is only called on a driver
    // reply tagged `INET_REP_OK`), so the mode does not move.
    assert!(!registry.note_inet_setopts(
        id,
        &record(INET_LOPT_ACTIVE, INET_PASSIVE),
        false
    ));
    assert!(registry.inet_active(id), "a refused `setopts` changes nothing");
}

#[test]
fn the_exit_on_close_mirror_defaults_to_true_and_follows_the_record() {
    let mut registry = bare_registry();
    let id = PortId::new(2);
    // The driver's own default: `exitf = 1` (`inet_drv.c:10929`).
    assert!(
        registry.inet_exit_on_close(id),
        "a port never told otherwise dies with its peer, as `exitf = 1` says"
    );
    assert!(registry.note_inet_setopts(id, &record(INET_LOPT_EXITONCLOSE, 0), true));
    assert!(
        !registry.inet_exit_on_close(id),
        "`{{exit_on_close,false}}` keeps the socket half-open"
    );
    assert!(registry.note_inet_setopts(id, &record(INET_LOPT_EXITONCLOSE, 1), true));
    assert!(
        registry.inet_exit_on_close(id),
        "`{{exit_on_close,true}}` restores the death"
    );
}

#[test]
fn the_once_only_latch_forgets_a_recycled_port_number() {
    let mut extras = crate::port::ops::PortExtras::default();
    extras.peer_closes.mark_reported(7);
    assert!(extras.peer_closes.is_reported(7));
    assert_eq!(extras.peer_closes.len(), 1);
    // A close drops the latch with the rest of the port's extras, so a number `beam_io` recycles
    // inherits neither a report nor an observation.
    extras.forget_port(7);
    assert!(!extras.peer_closes.is_reported(7));
    assert_eq!(extras.peer_closes.len(), 0);
    // The safety net: a latch whose port the caller no longer holds is pruned.
    extras.peer_closes.mark_reported(8);
    extras.peer_closes.mark_reported(9);
    assert_eq!(extras.peer_closes.prune(|number| number == 8), 1);
    assert!(extras.peer_closes.is_reported(8));
    assert!(!extras.peer_closes.is_reported(9));
    // And the registry-level counterpart is idempotent.
    let mut registry = bare_registry();
    registry.forget_peer_close(8);
    registry.forget_peer_close(8);
}

// -----------------------------------------------------------------------------------------------
// A live runtime, its port provider, and the one port under test
// -----------------------------------------------------------------------------------------------

/// A live [`Runtime`] plus the port provider built over the runtime's **own** registry, so the
/// port opened here is the port the runtime's `port_control` path, its `pump` and its peer-close
/// drain all see.
struct Session {
    rt: Runtime,
    fx: Fixture,
    hook: RuntimePorts,
    port: PortId,
}

impl Session {
    fn bare() -> Session {
        let fx = fixture();
        let rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let owner = rt.self_pid();
        let hook = fx.hook_over(Rc::clone(&rt.ports), owner);
        Session {
            rt,
            fx,
            hook,
            port: PortId::new(0),
        }
    }

    /// A `tcp_inet` port opened, `open`(2)'d and **connected** to a real loopback listener, plus
    /// the accepted `std::net` peer it is connected to.
    fn connected_tcp(label: &str) -> Option<(Session, std::net::TcpStream)> {
        let mut session = Session::bare();
        let port = session.open("tcp_inet", label)?;
        session.port = port;
        let peer = session.connect_loopback(label)?;
        Some((session, peer))
    }

    /// `open`(2)s the port under test as a stream socket, connects it to a fresh loopback
    /// listener and answers the accepted `std::net` peer — the other end a case closes.
    fn connect_loopback(&mut self, label: &str) -> Option<std::net::TcpStream> {
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) => {
                skip_no_loopback(&format!(
                    "{label}: TcpListener::bind(127.0.0.1:0) failed: {error}"
                ));
                return None;
            }
        };
        let Some(address) = listener.local_addr().ok() else {
            skip_no_loopback(&format!("{label}: the loopback listener has no local address"));
            return None;
        };
        assert_eq!(
            self.control("open", &[INET_AF_INET, INET_SOCK_STREAM, INET_PROTO_DEFAULT]),
            vec![1],
            "{label}: INET_REQ_OPEN on a stream/inet port answers the bare `[1]`"
        );
        assert_eq!(
            self.control("connect", &loopback_address(address.port())),
            vec![1],
            "{label}: a completed loopback connect answers the bare `[1]`"
        );
        match listener.accept() {
            Ok((peer, _)) => Some(peer),
            Err(error) => {
                skip_no_loopback(&format!("{label}: the loopback accept failed: {error}"));
                None
            }
        }
    }
    /// A `udp_inet` port opened and bound to a kernel-assigned loopback port, plus that port
    /// number.
    fn bound_udp(label: &str) -> Option<(Session, u16)> {
        let mut session = Session::bare();
        let port = session.open("udp_inet", label)?;
        session.port = port;
        assert_eq!(
            session.control("open", &[INET_AF_INET, INET_SOCK_DGRAM, INET_PROTO_DEFAULT]),
            vec![1],
            "{label}: INET_REQ_OPEN on a datagram/inet port answers the bare `[1]`"
        );
        let bound = session.control("bind", &[4, 0, 0]);
        assert_eq!(bound.len(), 3, "{label}: bind reports the port: {bound:?}");
        Some((session, u16::from_be_bytes([bound[1], bound[2]])))
    }

    /// Opens `{spawn_driver, driver}` with the `[binary]` stream option — the shape
    /// `prim_inet:open/6` always creates.
    fn open(&mut self, driver: &str, label: &str) -> Option<PortId> {
        let name = {
            let driver = self.fx.string(driver);
            self.fx.name("spawn_driver", driver)
        };
        let options = {
            let binary = self.fx.atom("binary");
            self.fx.list(&[binary])
        };
        match self.hook.open_port(name, options) {
            Ok(port) => Some(port),
            Err(error) => {
                skip_no_loopback(&format!(
                    "{label}: open_port({{spawn_driver,\"{driver}\"}}) failed: {error}"
                ));
                None
            }
        }
    }

    /// Runs one control op on the port under test.
    fn control(&mut self, verb: &str, arg: &[u8]) -> Vec<u8> {
        let port = self.port;
        self.rt
            .ports
            .borrow_mut()
            .port_control(port, verb, arg)
            .unwrap_or_else(|error| panic!("the {verb:?} control op on {port:?}: {error}"))
    }

    /// Applies a `setopts` record list through the numeric request, returning the driver's reply.
    ///
    /// The mirror is fed by production code ([`crate::port::PortRegistry::port_control`] calls
    /// `note_inet_setopts`); the explicit call below only runs when the hook moved no latch, so
    /// this helper cannot double-record and stays correct either way.
    fn setopts(&mut self, arg: &[u8]) -> Vec<u8> {
        let port = self.port;
        let before = self.recorded();
        let reply = self.control(&setopts_op(), arg);
        let ok = setopts_succeeded(&reply);
        if ok && self.recorded() == before {
            self.rt.ports.borrow_mut().note_inet_setopts(port, arg, ok);
        }
        reply
    }

    /// One **passive read** of whatever the socket holds — the C's `TCP_REQ_RECV`(42), the request
    /// `prim_inet:recv/3` sends (`inet_drv.c:12265-12325`), answering the driver's enqueue reply
    /// (`[1] ++ Id`, or the `[0] ++ "einval"` shape when the live mode is not passive).
    ///
    /// This is the only way to make a socket whose `{active, …}` grant is spent read at all: the
    /// delivery that empties the grant deselects its read side
    /// (`sock_select(INETP(desc), (FD_READ|FD_CLOSE), 0)`, `:13106`-`:13114`), and the enqueue's
    /// own `sock_select(…, 1)` (`:12315`) is what arms it again. The bytes stay unread: the
    /// completion rides the port's async channel, which these cases do not consume — the
    /// *observation* is the point.
    fn passive_recv(&mut self) -> Vec<u8> {
        let op = (INET_CONTROL_MAGIC + TCP_REQ_RECV).to_string();
        let mut arg = Vec::with_capacity(8);
        arg.extend_from_slice(&0u32.to_be_bytes()); // Timeout(4) — no timer for this read
        arg.extend_from_slice(&0u32.to_be_bytes()); // Length(4) — "whatever is there"
        self.control(&op, &arg)
    }

    /// How many of the port's three inet latches are set — the hook-detection witness
    /// [`Session::setopts`] reads across the control call.
    ///
    /// The mirror keeps three latches (`{mode, …}`, `{active, …}`, `{exit_on_close, …}`) and no
    /// record list (`src/port/inet_opts.rs`), so a `setopts` naming none of those three leaves the
    /// witness unmoved and the stand-in makes its call again. That cannot move a latch — the
    /// production hook has already recorded the same argument, and each latch is last-value-wins
    /// over the very same bytes.
    fn recorded(&self) -> usize {
        let port = self.port;
        self.rt
            .ports
            .borrow()
            .inet_options(port)
            .map_or(0, |options| {
                usize::from(options.mode().is_some())
                    + usize::from(options.active_mode().is_some())
                    + usize::from(options.exit_on_close().is_some())
            })
    }

    /// One pump round, then one read of the peer-close channel — the two steps a consuming route
    /// performs, in the order `src/port.rs` performs them.
    fn round(&mut self) -> Vec<PeerCloseReport> {
        self.pump_once();
        self.rt.ports.borrow_mut().drain_peer_closes()
    }

    /// One port round and **no** drain: the step that makes the driver see a peer's EOF, for the
    /// cases whose drain is the delivery half under test (a full
    /// [`PortRegistry::drain_peer_closes`](crate::port::PortRegistry) here would consume the
    /// report before it could be).
    fn pump_once(&mut self) {
        let _ = {
            let mut registry = self.rt.ports.borrow_mut();
            registry
                .pump(PUMP_SLICE)
                .unwrap_or_else(|error| panic!("the port round: {error}"))
        };
    }

    /// Whether the driver has observed the port's peer close (a latched, sticky answer) — the
    /// condition that ends a pump-until-observed loop without waiting out the whole bound.
    fn peer_close_observed(&self) -> bool {
        self.rt
            .ports
            .borrow()
            .registry
            .port_closed(self.port)
            .unwrap_or(false)
    }

    /// The **live** `{active, …}` state the driver holds — `beam_io::PortRegistry::active_mode`,
    /// i.e. the C's `desc->inet.active` (+ `active_count`) *after* every granted delivery spent its
    /// grant (`INET_CHECK_ACTIVE_TO_PASSIVE`, `inet_drv.c:1552-1560`). This is the exact store
    /// [`PortRegistry::inet_active`](super::PortRegistry) reads for the peer-close gate, so a case
    /// that asserts on it proves the deliver-before-close sequence really spent the grant instead of
    /// silently exercising the never-armed socket.
    fn active_mode(&self) -> ActiveMode {
        let port = self.port;
        self.rt
            .ports
            .borrow()
            .registry
            .active_mode(port)
            .unwrap_or_else(|error| panic!("the driver's {{active,…}} state for {port:?}: {error}"))
    }

    /// One port round plus every `{active, …}` delivery it staged, in the driver's order — the
    /// `{tcp, S, Data}` of a granted read always precedes the `{tcp_passive, S}` that read spent
    /// (`INET_CHECK_ACTIVE_TO_PASSIVE` runs after `tcp_message`, `inet_drv.c:1552-1560`).
    ///
    /// Unlike [`Session::round`] this does **not** touch the peer-close channel, so pumping for a
    /// delivery never consumes a report another assertion is about to read.
    fn active_round(&mut self) -> Vec<ActiveInputPacket> {
        let events = {
            let mut registry = self.rt.ports.borrow_mut();
            registry
                .pump(PUMP_SLICE)
                .unwrap_or_else(|error| panic!("the port round: {error}"))
        };
        events
            .into_iter()
            .filter_map(|event| match event {
                PortEvent::ActiveInput { packet } => Some(packet),
                _ => None,
            })
            .collect()
    }

    /// Pumps until `want` deliveries are staged or `bound` expires, answering **everything** staged
    /// (a delivery that spends an `{active, N}` counter stages its data message *and* the passive
    /// message in the same round). `want = 1` over a quiet socket is therefore the "nothing was
    /// delivered" probe.
    fn deliveries(&mut self, bound: Duration, want: usize) -> Vec<ActiveInputPacket> {
        let deadline = Instant::now() + bound;
        let mut packets = Vec::new();
        loop {
            packets.extend(self.active_round());
            if packets.len() >= want || Instant::now() >= deadline {
                return packets;
            }
        }
    }
    /// Pumps until the channel reports something or `bound` expires.
    fn collect(&mut self, bound: Duration) -> Vec<PeerCloseReport> {
        let deadline = Instant::now() + bound;
        loop {
            let reports = self.round();
            if !reports.is_empty() || Instant::now() >= deadline {
                return reports;
            }
        }
    }

    /// Polls for [`QUIET_WINDOW`] and returns everything the channel reported.
    fn quiet(&mut self) -> Vec<PeerCloseReport> {
        let deadline = Instant::now() + QUIET_WINDOW;
        let mut reports = Vec::new();
        loop {
            reports.extend(self.round());
            if Instant::now() >= deadline {
                return reports;
            }
        }
    }

    /// `report`'s message rendered by the registered `~w` printer — the text the oracle's own
    /// `io:format("~w",[Message])` prints.
    fn render(&mut self, report: &PeerCloseReport) -> String {
        let term = {
            let mut context = RuntimeContext::new(&mut self.rt);
            context
                .peer_close_message(report)
                .unwrap_or_else(|error| panic!("a peer-close message for {report:?}: {error}"))
        };
        self.rt
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a built term: {error}"))
            .trim_end_matches('\n')
            .to_string()
    }

    /// The port's term as the oracle prints it (`#Port<0.N>`).
    fn port_text(&mut self) -> String {
        let term = self.port.to_eterm();
        self.rt
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a port term: {error}"))
            .trim_end_matches('\n')
            .to_string()
    }

    /// `packet`'s message — `{tcp, S, Data}` / `{tcp_passive, S}` — rendered by the registered `~w`
    /// printer, the text the oracle's own `io:format("~w",[Message])` prints.
    fn render_active(&mut self, packet: &ActiveInputPacket) -> String {
        let term = {
            let mut context = RuntimeContext::new(&mut self.rt);
            context
                .active_input_message(packet)
                .unwrap_or_else(|error| panic!("an active-input message for {packet:?}: {error}"))
        };
        self.rt
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a built term: {error}"))
            .trim_end_matches('\n')
            .to_string()
    }
    /// One **observe-and-deliver** step of the scheduler route: a port round (which is what makes
    /// the driver see the peer's EOF at all) followed by the delivery half under test, with `table`
    /// standing in for the process table a scheduler pass moves out of the runtime.
    fn deliver(&mut self, table: &mut ProcessTable) -> usize {
        let _ = {
            let mut registry = self.rt.ports.borrow_mut();
            registry
                .pump(PUMP_SLICE)
                .unwrap_or_else(|error| panic!("the port round: {error}"))
        };
        self.rt
            .deliver_peer_closes(table)
            .unwrap_or_else(|error| panic!("the peer-close delivery: {error}"))
    }

    /// Repeats [`Session::deliver`] until it delivers something or `bound` expires.
    fn deliver_until(&mut self, table: &mut ProcessTable, bound: Duration) -> usize {
        let deadline = Instant::now() + bound;
        loop {
            let delivered = self.deliver(table);
            if delivered > 0 || Instant::now() >= deadline {
                return delivered;
            }
        }
    }
}

/// A [`Session`] whose port is owned by a process living in a **separate** process table.
///
/// `Runtime::deliver_peer_closes` (like `Runtime::deliver_port_signals`) takes that table
/// alongside the runtime — the scheduler pass moves it out of the [`Runtime`], so `&mut
/// rt.processes` cannot be handed to `&mut rt` — which means the mailbox-writing half can only be
/// exercised against a table the test owns. The provider records `owner` as the port's connected
/// process, exactly as `open_port/2` does for a real one.
///
/// The owner's **number** is deliberately not the runtime's own (`rt.self_pid()`): a local pid is
/// identified by its number alone, so a fresh table's first process would be `=:=` the runtime's
/// root and could not stand in for "somebody else" ([`RuntimeContext::poll_peer_closes`] takes
/// exactly the reports addressed to its own pid). One extra spawn makes the owner genuinely
/// foreign.
fn table_owned(label: &str) -> Option<(Session, ProcessTable, ProcessId)> {
    let fx = fixture();
    let rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
    let mut owner = table.spawn_default().expect("the port's owner");
    while owner == rt.self_pid() {
        owner = table.spawn_default().expect("a distinct owner");
    }
    let hook = fx.hook_over(Rc::clone(&rt.ports), owner);
    let mut session = Session {
        rt,
        fx,
        hook,
        port: PortId::new(0),
    };
    let port = session.open("tcp_inet", label)?;
    session.port = port;
    Some((session, table, owner))
}

/// Every message in `pid`'s mailbox, as terms.
fn mailbox_terms(table: &ProcessTable, pid: ProcessId) -> Vec<Eterm> {
    table
        .get(pid)
        .unwrap_or_else(|| panic!("the recipient {pid:?}"))
        .mailbox()
        .iter()
        .map(|message| message.payload())
        .collect()
}

/// `term` as the registered `~w` printer renders it — the text the oracle's own
/// `io:format("~w",[Term])` prints, minus the trailing newline.
fn rendered(rt: &mut Runtime, term: Eterm) -> String {
    rt.format_term_w(term, None)
        .unwrap_or_else(|error| panic!("the `~w` printer refused a built term: {error}"))
        .trim_end_matches('\n')
        .to_string()
}

// -----------------------------------------------------------------------------------------------
// The real-traffic cases
// -----------------------------------------------------------------------------------------------

/// An **active** socket whose peer closes reports `{tcp_closed, S}` exactly **once**, to its
/// connected process, as a 2-tuple — the C's `tcp_recv_closed` active arm plus
/// `TCP_ADDF_CLOSE_SENT`.
#[test]
fn an_active_tcp_socket_reports_the_peer_close_exactly_once() {
    let Some((mut session, peer)) = Session::connected_tcp("peer close: active TCP") else {
        return;
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    // The peer goes away: the driver's read path observes the EOF (`tcp_recv_closed`).
    drop(peer);
    let reports = session.collect(DELIVERY_BOUND);
    assert_eq!(
        reports.len(),
        1,
        "an active socket's peer close is exactly one report, got {reports:?}"
    );
    let report = reports[0];
    assert_eq!(report.port, session.port, "the report names its own port");
    assert_eq!(
        report.owner,
        session.rt.self_pid(),
        "`{{tcp_closed, S}}` goes to the port's **connected** process (the C's `desc->inet.dport`)"
    );
    assert_eq!(report.kind, PeerCloseKind::Tcp);
    assert!(
        report.exit_on_close,
        "the driver's `exitf` default is 1 (`inet_drv.c:10929`), so the port dies after the message"
    );
    let rendered = session.render(&report);
    assert_eq!(
        rendered,
        format!("{{tcp_closed,{}}}", session.port_text()),
        "the oracle prints `{{tcp_closed,#Port<0.N>}}` — a 2-tuple, never a 3"
    );
    // The once-only latch: another round (and another drain) reports nothing.
    let again = session.round();
    assert!(
        again.is_empty(),
        "`TCP_ADDF_CLOSE_SENT` allows one message per descriptor, got {again:?}"
    );
}

/// A **passive** socket's peer close reports **nothing** — and the very same latch reports the
/// moment an active mode is armed, which is the C's sticky re-fire (`inet_set_opts`'s
/// `INET_LOPT_ACTIVE` arm).
#[test]
fn a_passive_tcp_socket_reports_nothing_until_an_active_mode_is_armed() {
    let Some((mut session, peer)) = Session::connected_tcp("peer close: passive then active") else {
        return;
    };
    // Passive (the driver's default): the close is observed but answered with no message at all.
    drop(peer);
    let quiet = session.quiet();
    assert!(
        quiet.is_empty(),
        "`tcp_recv_closed`'s passive arm sends no message (`inet_drv.c:12819-12832`), got {quiet:?}"
    );
    // Arming an active mode on the already-closed descriptor makes the message due — once.
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}` on a closed descriptor"
    );
    let reports = session.collect(DELIVERY_BOUND);
    assert_eq!(
        reports.len(),
        1,
        "the sticky re-fire is exactly one report at the transition, got {reports:?}"
    );
    assert_eq!(reports[0].port, session.port);
    let again = session.round();
    assert!(
        again.is_empty(),
        "the re-fire is once-only too, got {again:?}"
    );
}

/// An **exhausted** `{active, N}` counter puts the socket back in the passive arm, so its peer's
/// close is silent — and the sticky re-fire is what owes the message once an active mode is armed
/// again.
///
/// The C's accounting: `INET_CHECK_ACTIVE_TO_PASSIVE`
/// (`erts/emulator/drivers/common/inet_drv.c:1552-1560`) spends one grant per delivered packet and,
/// on the delivery that drives an `INET_MULTI` counter to zero, flips `desc->inet.active` to
/// `INET_PASSIVE` **and** sends one `packet_passive_message`; an `INET_ONCE` grant is spent
/// silently, with no message at all. `tcp_recv_closed` then reads `if (!desc->inet.active)` and
/// takes its passive arm (`:12819-12832` — "passive mode do not terminate port !", no message)
/// instead of the active one (`:12833-12842`), and `inet_set_opts`'s `INET_LOPT_ACTIVE` arm
/// (`:6968-7016`, the sticky re-fire at `:7001-7015`) sends the `{tcp_closed, S}` the moment an
/// active mode is armed on the already-closed descriptor.
///
/// Measured on the oracle (OTP 29.1.1 / erts-17.1) for this sequence — `{active,2}`, two bytes one
/// at a time, close, one passive `gen_tcp:recv/3`, re-arm `{active,true}` (the port number is the
/// oracle's own):
///
/// ```text
/// one={tcp,#Port<0.5>,<<97>>}
/// two={tcp,#Port<0.5>,<<98>>}
/// passive={tcp_passive,#Port<0.5>}
/// silent=timeout
/// recv={error,closed}
/// recv_after=timeout
/// closed={tcp_closed,#Port<0.5>}
/// ```
///
/// The `recv` line is the observation this case pins: the spend deselected the read side
/// (`:13106`-`:13114`), so the close is only noticed once something reads — and a passive read
/// answers `closed` (`:12826`) while sending **no** message. A gate reading the last accepted
/// `setopts` instead of the live state would fabricate the `{tcp_closed, S}` right there.
///
/// and for the `{active,1}` form of the same sequence (`data` / `passive` / `silent` / `closed`):
///
/// ```text
/// data={tcp,#Port<0.5>,<<120>>}
/// passive={tcp_passive,#Port<0.5>}
/// silent=timeout
/// closed={tcp_closed,#Port<0.5>}
/// ```
#[test]
fn an_exhausted_active_n_counter_reports_nothing_until_an_active_mode_is_rearmed() {
    let Some((mut session, mut peer)) = Session::connected_tcp("peer close: spent counter") else {
        return;
    };
    // Two grants: the first delivery spends one (leaving 1), the second drives the counter to zero.
    assert_eq!(
        session.setopts(&multi_record(INET_LOPT_ACTIVE, INET_MULTI, 2)),
        vec![1],
        "the driver accepts `{{active,2}}`"
    );
    assert_eq!(
        session.active_mode(),
        ActiveMode::Multi { remaining: 2 },
        "two grants armed, none spent yet (the C's `active_count`, `inet_drv.c:6975-6987`)"
    );

    assert!(
        peer.write_all(b"a").is_ok(),
        "the peer writes the first granted byte"
    );
    let first = session.deliveries(DELIVERY_BOUND, 1);
    assert_eq!(
        first.len(),
        1,
        "one granted read is one message, got {first:?}"
    );
    let port_text = session.port_text();
    assert_eq!(
        session.render_active(&first[0]),
        format!("{{tcp,{port_text},<<97>>}}"),
        "`tcp_message`'s `{{tcp, S, Data}}` (`inet_drv.c:3979`), the binary the `[binary]` port \
         renders — `~w` prints a binary's bytes in decimal, as the oracle does"
    );
    assert_eq!(
        session.active_mode(),
        ActiveMode::Multi { remaining: 1 },
        "the delivery spent exactly one grant (`:1556`), so the counter is 1 and the mode is still active"
    );

    assert!(
        peer.write_all(b"b").is_ok(),
        "the peer writes the second granted byte"
    );
    let rest = session.deliveries(DELIVERY_BOUND, 2);
    assert_eq!(
        rest.len(),
        2,
        "the delivery that spends the last grant stages its data message and the passive message \
         together (`:1556-1559`), got {rest:?}"
    );
    assert!(
        matches!(rest[0].event, ActiveInputEvent::Data(_)),
        "the data message comes first, got {:?}",
        rest[0].event
    );
    assert_eq!(
        session.render_active(&rest[0]),
        format!("{{tcp,{port_text},<<98>>}}"),
        "the second granted byte is its own message"
    );
    assert_eq!(
        rest[1].event,
        ActiveInputEvent::Passive,
        "the `INET_MULTI` arm's zero is the only place `packet_passive_message` fires (`:1558`)"
    );
    assert_eq!(
        session.render_active(&rest[1]),
        format!("{{tcp_passive,{port_text}}}"),
        "the oracle's `{{tcp_passive, S}}` for the exhausted counter"
    );
    assert_eq!(
        session.active_mode(),
        ActiveMode::False,
        "the zeroed counter cleared the mode (`:1557`), which is the state `tcp_recv_closed` tests"
    );

    // The C's passive arm: no message and no port death (`:12819-12832`).
    drop(peer);
    let quiet = session.quiet();
    assert!(
        quiet.is_empty(),
        "an exhausted `{{active,N}}` grant leaves the socket passive, so the close reports nothing \
         (`inet_drv.c:12819-12832`), got {quiet:?}"
    );
    // The witness the empty channel needs: the driver **did** observe the EOF while the socket was
    // passive (`tcp_inet_input` reads on the readiness event whatever the mode,
    // `inet_drv.c:13598-13600`, and `tcp_recv_closed` only then reads `desc->inet.active`). Without
    // this the silence above would also hold for a close nobody had seen yet, i.e. for a gate that
    // never ran — `session.active_mode()` already proves the store the gate reads is passive.
    //
    // The passive window alone does **not** observe it: the delivery that emptied the counter
    // deselected the socket's read side (`sock_select(INETP(desc), (FD_READ|FD_CLOSE), 0)`,
    // `:13106`-`:13114`), so the EOF stays in the kernel until something arms the read again. A
    // passive read is what does — the C's `TCP_REQ_RECV`(42) enqueue (`sock_select(…, 1)`,
    // `:12315`), whose read answers `0` and runs `tcp_recv_closed` with `desc->inet.active == 0`.
    assert!(
        !session.peer_close_observed(),
        "a spent grant leaves the socket deselected (`:13106`-`:13114`), so the silent window reads \
         nothing and cannot have observed the peer's EOF yet"
    );
    let queued = session.passive_recv();
    assert_eq!(
        queued.first().copied(),
        Some(1),
        "the passive read is queued (`[1] ++ Id`, `:12308`): the driver's own `desc->inet.active` is \
         passive, which is the `:12290` guard, got {queued:?}"
    );
    let after_read = session.quiet();
    assert!(
        after_read.is_empty(),
        "the close is observed with the socket passive, so the C takes the silent arm \
         (`inet_drv.c:12819-12832`) and answers the *read* with `closed` (`:12826`) instead, got \
         {after_read:?}"
    );
    assert!(
        session.peer_close_observed(),
        "the passive read is what latched the peer's EOF — this is the close the gate must suppress"
    );
    assert!(
        session.rt.ports.borrow().is_live(session.port),
        "the passive arm does not terminate the port (`:12820-12821` \"passive mode do not \
         terminate port !\")"
    );

    // The sticky re-fire: an active mode on the already-closed descriptor makes the message due.
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ACTIVE)),
        vec![1],
        "the driver accepts `{{active,true}}` on a closed descriptor"
    );
    let reports = session.collect(DELIVERY_BOUND);
    assert_eq!(
        reports.len(),
        1,
        "the re-fire is exactly one report (`:7001-7015` + `TCP_ADDF_CLOSE_SENT`), got {reports:?}"
    );
    assert_eq!(reports[0].port, session.port, "the report names its own port");
    assert_eq!(
        session.render(&reports[0]),
        format!("{{tcp_closed,{port_text}}}"),
        "the oracle prints `{{tcp_closed,#Port<0.N>}}` — a 2-tuple, never a 3"
    );
    let again = session.round();
    assert!(
        again.is_empty(),
        "the re-fire is once-only too, got {again:?}"
    );
}

/// The `{active, once}` twin: the single granted read spends the mode **silently** — no
/// `{tcp_passive, S}`, which the C sends only for `INET_MULTI` — so the close that follows is
/// silent, and a re-arm is what owes the one `{tcp_closed, S}`.
///
/// `INET_CHECK_ACTIVE_TO_PASSIVE` (`erts/emulator/drivers/common/inet_drv.c:1552-1560`) spends an
/// `INET_ONCE` grant by flipping the mode with **no** message (`:1554-1555`); the exhausted socket
/// is therefore passive at `tcp_recv_closed` and takes the silent arm (`:12819-12832`); the sticky
/// re-fire at `:7001-7015` sends the message when an active mode is armed afterwards.
///
/// Measured on the oracle (OTP 29.1.1 / erts-17.1) for this sequence — `{active,once}`, one byte,
/// close, one passive `gen_tcp:recv/3`, re-arm `{active,true}`:
///
/// ```text
/// data={tcp,#Port<0.5>,<<120>>}
/// passive=timeout
/// silent=timeout
/// recv={error,closed}
/// recv_after=timeout
/// closed={tcp_closed,#Port<0.5>}
/// ```
///
/// `recv_after=timeout` with a `recv={error,closed}` above it is the same observation as the
/// `{active,N}` twin's: the passive read is what discovers the EOF, the passive arm answers the
/// *read* and sends nothing, and only the re-arm owes the `{tcp_closed, S}`.
#[test]
fn a_spent_active_once_socket_reports_nothing_until_an_active_mode_is_rearmed() {
    let Some((mut session, mut peer)) = Session::connected_tcp("peer close: spent once") else {
        return;
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    assert!(
        peer.write_all(b"x").is_ok(),
        "the peer writes the single granted byte"
    );
    let delivered = session.deliveries(DELIVERY_BOUND, 1);
    assert_eq!(
        delivered.len(),
        1,
        "one grant is one message, got {delivered:?}"
    );
    let port_text = session.port_text();
    assert_eq!(
        session.render_active(&delivered[0]),
        format!("{{tcp,{port_text},<<120>>}}"),
        "`tcp_message`'s `{{tcp, S, Data}}`"
    );
    assert_eq!(
        session.active_mode(),
        ActiveMode::False,
        "the read spent the grant (`:1554-1555`), so the driver is passive with no counter left"
    );
    let extra = session.deliveries(QUIET_WINDOW, 1);
    assert!(
        extra.is_empty(),
        "`INET_ONCE` never sends `{{tcp_passive, S}}` — only the `INET_MULTI` arm does (`:1556-1558`) \
         — got {extra:?}"
    );

    // Passive now: the peer's close is the C's silent passive arm (`:12819-12832`).
    drop(peer);
    let quiet = session.quiet();
    assert!(
        quiet.is_empty(),
        "a spent `{{active,once}}` socket reports nothing at the close (`inet_drv.c:12819-12832`), \
         got {quiet:?}"
    );
    // As in the `{{active,N}}` twin: the silence is only evidence if the close was **observed** while
    // the socket was passive — the read happens on readiness whatever the mode
    // (`inet_drv.c:13598-13600`), and `session.active_mode()` already proved the grant is gone. The
    // spend deselected the read side (`:13106`-`:13114`), so the passive window observes nothing and
    // the `TCP_REQ_RECV`(42) enqueue's own `sock_select(…, 1)` (`:12315`) is what arms it again.
    assert!(
        !session.peer_close_observed(),
        "the spent grant deselected the socket's read side (`:13106`-`:13114`), so the silent window \
         cannot have observed the peer's EOF yet"
    );
    let queued = session.passive_recv();
    assert_eq!(
        queued.first().copied(),
        Some(1),
        "the passive read is queued (`[1] ++ Id`, `:12308`), got {queued:?}"
    );
    let after_read = session.quiet();
    assert!(
        after_read.is_empty(),
        "a close observed while passive is the silent arm (`inet_drv.c:12819-12832`) — the read \
         takes the `closed` answer (`:12826`) and no `{{tcp_closed, S}}` exists, got {after_read:?}"
    );
    assert!(
        session.peer_close_observed(),
        "the passive read is what latched the peer's EOF — a close nobody has seen yet would make \
         the empty channel meaningless"
    );
    assert!(
        session.rt.ports.borrow().is_live(session.port),
        "the passive arm does not terminate the port (`:12820-12821` \"passive mode do not \
         terminate port !\")"
    );

    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ACTIVE)),
        vec![1],
        "the driver accepts `{{active,true}}` on a closed descriptor"
    );
    let reports = session.collect(DELIVERY_BOUND);
    assert_eq!(
        reports.len(),
        1,
        "the sticky re-fire is exactly one report (`:7001-7015`), got {reports:?}"
    );
    assert_eq!(reports[0].port, session.port, "the report names its own port");
    assert_eq!(
        session.render(&reports[0]),
        format!("{{tcp_closed,{port_text}}}"),
        "the oracle prints `{{tcp_closed,#Port<0.N>}}`"
    );
    let again = session.round();
    assert!(
        again.is_empty(),
        "the re-fire is once-only too, got {again:?}"
    );
}
/// A `udp_inet` port never reports a peer close: `inet_drv` has no `{udp_closed, _}` message.
#[test]
fn a_udp_inet_port_never_reports_a_peer_close() {
    let Some((mut session, number)) = Session::bound_udp("peer close: datagram port") else {
        return;
    };
    let Some(sender) = std::net::UdpSocket::bind("127.0.0.1:0").ok() else {
        skip_no_loopback("peer close: datagram port: UdpSocket::bind failed");
        return;
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    let target = format!("127.0.0.1:{number}");
    assert!(
        sender.send_to(b"hi", &target).is_ok(),
        "a real datagram reaches the bound port"
    );
    drop(sender);
    let reports = session.quiet();
    assert!(
        reports.is_empty(),
        "a datagram port has no close message at all (`rg udp_closed erts/` is empty), got \
         {reports:?}"
    );
    assert!(
        session.rt.ports.borrow().is_live(session.port),
        "the datagram port is still live — nothing here may retire it"
    );
}

/// The driver-kind gate: only a `tcp_inet` port is consulted, and the atom family's tag is the C's
/// `tcp_closed`.
#[test]
fn the_message_tag_is_the_cs_tcp_closed_atom() {
    let mut session = Session::bare();
    let report = PeerCloseReport {
        port: PortId::new(1),
        owner: session.rt.self_pid(),
        kind: PeerCloseKind::Tcp,
        exit_on_close: true,
    };
    let rendered = session.render(&report);
    assert!(
        rendered.starts_with("{tcp_closed,"),
        "the family's tag is `tcp_closed`: {rendered}"
    );
    assert_eq!(
        terms::tuple_arity(
            RuntimeContext::new(&mut session.rt)
                .peer_close_message(&report)
                .expect("the peer-close message")
        )
        .expect("a tuple's arity"),
        2,
        "`{{tcp_closed, S}}` is a 2-tuple"
    );
}

// -----------------------------------------------------------------------------------------------
// The retirement half — the C's `driver_exit`, not `port_close/1`
// -----------------------------------------------------------------------------------------------

/// A peer-closed port is retired with **`driver_exit`'s** signal set: every monitor gets its
/// `{'DOWN', Ref, port, S, normal}`, and a **trapping linked owner gets nothing at all**.
///
/// That asymmetry is the whole point of [`PortRegistry::retire_peer_closed`] and the reason it
/// cannot simply be `port_close/1`: `driver_exit` runs `unlink_proc` first (`io.c:7244`), so the
/// link family drops the owner, while the C's one `erts_deliver_port_exit` path still tells the
/// monitors. Measured on the oracle (OTP 29.1.1 / erts-17.1): for a socket opened with
/// `{active,true}` whose peer closes, a `trap_exit` owner's mailbox stays **empty** and the port
/// is gone afterwards (`erlang:port_info(S, connected)` ⇒ `undefined`); a
/// `monitor(port, S)` watcher receives `{'DOWN',Ref,port,S,normal}`.
#[test]
fn retiring_a_peer_closed_port_tells_its_monitors_and_not_its_owner() {
    let Some((mut session, mut table, owner)) = table_owned("peer close: driver_exit") else {
        return;
    };
    let watcher = table.spawn_default().expect("the port's watcher");
    // `open_port/2` links the port to its owner, so an ordinary `port_close/1` would hand this
    // trapping process `{'EXIT', S, normal}` — which `driver_exit` must not.
    table
        .get_mut(owner)
        .expect("the owner")
        .set_trap_exit(true);
    {
        let mut registry = session.rt.ports.borrow_mut();
        table
            .monitor_port(watcher, session.port, registry.port_table_mut())
            .expect("a monitor on the port");
    }

    session
        .rt
        .ports
        .borrow_mut()
        .retire_peer_closed(session.port)
        .expect("the peer-close retirement");

    assert!(
        !session.rt.ports.borrow().is_live(session.port),
        "`driver_exit(desc->inet.port, 0)` kills the port: it is gone from the table"
    );

    // The `'DOWN'`s become deliverable at the port's close **report**, which a round mints — the
    // same two-step shape every close's signals have (see `exit.rs`).
    let _ = {
        let mut registry = session.rt.ports.borrow_mut();
        registry
            .pump(PUMP_SLICE)
            .unwrap_or_else(|error| panic!("the port round: {error}"))
    };
    let delivered = session
        .rt
        .deliver_port_signals(&mut table)
        .expect("the close's signals");
    assert_eq!(
        delivered, 1,
        "exactly one message is owed: the watcher's `'DOWN'` — `driver_exit` unlinked the owner"
    );

    assert!(
        mailbox_terms(&table, owner).is_empty(),
        "a trapping linked owner receives NOTHING when the peer close kills the port \
         (the oracle's own answer — `driver_exit` runs `unlink_proc` before delivering)"
    );
    let down = mailbox_terms(&table, watcher);
    assert_eq!(down.len(), 1, "one `'DOWN'` per watcher, got {down:?}");
    let port_text = session.port_text();
    let rendered = rendered(&mut session.rt, down[0]);
    assert!(
        rendered.starts_with("{'DOWN',")
            && rendered.ends_with(&format!(",port,{port_text},normal}}")),
        "the oracle's `{{'DOWN', Ref, port, S, normal}}`, got {rendered}"
    );
    assert_eq!(
        terms::tuple_arity(down[0]).expect("a tuple's arity"),
        5,
        "`{{'DOWN', Ref, port, S, normal}}` is a 5-tuple"
    );
}

/// The scheduler route writes `{tcp_closed, S}` into the **connected process's** mailbox, once,
/// and then retires the port (`exit_on_close` defaults to `true`).
///
/// The mailbox shape is the oracle's: the *only* message an active socket's peer close produces is
/// the bare 2-tuple `{tcp_closed,S}` — never a `{tcp_passive,S}` beside it, never a 3-tuple.
#[test]
fn the_scheduler_route_delivers_the_peer_close_into_the_owners_mailbox() {
    let Some((mut session, mut table, owner)) = table_owned("peer close: delivery") else {
        return;
    };
    let peer = match session.connect_loopback("peer close: delivery") {
        Some(peer) => peer,
        None => return,
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    drop(peer);

    let delivered = session.deliver_until(&mut table, DELIVERY_BOUND);
    assert_eq!(
        delivered, 1,
        "one peer close delivers one `{{tcp_closed, S}}` to the connected process"
    );
    let messages = mailbox_terms(&table, owner);
    assert_eq!(messages.len(), 1, "one message, got {messages:?}");
    let port_text = session.port_text();
    assert_eq!(
        rendered(&mut session.rt, messages[0]),
        format!("{{tcp_closed,{port_text}}}"),
        "the oracle prints `{{tcp_closed,#Port<0.N>}}` — the bare 2-tuple"
    );
    assert!(
        !session.rt.ports.borrow().is_live(session.port),
        "`exitf` defaults to 1, so the port dies right after the message (`inet_drv.c:12835-12837`)"
    );
    assert_eq!(
        session
            .rt
            .deliver_peer_closes(&mut table)
            .expect("a second delivery pass"),
        0,
        "`TCP_ADDF_CLOSE_SENT`: the message is delivered at most once per descriptor"
    );
}

/// The `-eval` route returns the message for **its own** process and leaves the port dead.
#[test]
fn poll_peer_closes_returns_the_message_addressed_to_this_process() {
    let Some((mut session, peer)) = Session::connected_tcp("peer close: eval route") else {
        return;
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    drop(peer);

    // The evaluator sees its own EOF only through a port round, then takes its message inline.
    // The round is [`Session::pump_once`], never [`Session::round`]: the latter also drains the
    // channel, which would consume the very report this case is about to poll for.
    let deadline = Instant::now() + DELIVERY_BOUND;
    while !session.peer_close_observed() && Instant::now() < deadline {
        session.pump_once();
    }
    assert!(
        session.peer_close_observed(),
        "the driver must have latched the peer's EOF before the poll can report it"
    );
    let messages = {
        let mut ctx = RuntimeContext::new(&mut session.rt);
        ctx.poll_peer_closes()
            .unwrap_or_else(|error| panic!("the `-eval` peer-close poll: {error}"))
    };
    let port_text = session.port_text();
    assert_eq!(
        messages.len(),
        1,
        "the evaluator owns the socket, so the report is its own"
    );
    let (sender, message) = messages[0];
    assert_eq!(
        rendered(&mut session.rt, sender),
        port_text,
        "the sender is the port itself, as on the mailbox route"
    );
    assert_eq!(
        rendered(&mut session.rt, message),
        format!("{{tcp_closed,{port_text}}}"),
        "the `-eval` route builds the same 2-tuple the mailbox route delivers"
    );
    assert!(
        !session.rt.ports.borrow().is_live(session.port),
        "the retirement runs on this route too — the evaluator's port is gone"
    );
}

/// A report addressed to **another** process is left latched by the `-eval` route, so the
/// scheduler route still delivers it — never dropped between the two routes.
#[test]
fn poll_peer_closes_leaves_another_processs_message_latched() {
    let Some((mut session, mut table, owner)) = table_owned("peer close: foreign owner") else {
        return;
    };
    let peer = match session.connect_loopback("peer close: foreign owner") {
        Some(peer) => peer,
        None => return,
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    drop(peer);

    let deadline = Instant::now() + DELIVERY_BOUND;
    while !session.peer_close_observed() && Instant::now() < deadline {
        session.pump_once();
    }
    assert!(
        session.peer_close_observed(),
        "the driver must have latched the peer's EOF before the poll can decline it"
    );
    let polled = {
        let mut ctx = RuntimeContext::new(&mut session.rt);
        ctx.poll_peer_closes()
            .unwrap_or_else(|error| panic!("the `-eval` peer-close poll: {error}"))
    };
    assert!(
        polled.is_empty(),
        "the socket belongs to another process, so the evaluator returns nothing: {polled:?}"
    );
    assert!(
        session.rt.ports.borrow().is_live(session.port),
        "and it does not retire a port whose message it never took"
    );

    let delivered = session
        .rt
        .deliver_peer_closes(&mut table)
        .expect("the scheduler route's delivery");
    assert_eq!(
        delivered, 1,
        "the latch was left undrained, so the route that owns the process table still reports it"
    );
    let port_text = session.port_text();
    let messages = mailbox_terms(&table, owner);
    assert_eq!(
        messages.len(),
        1,
        "the message reaches the port's real owner, got {messages:?}"
    );
    assert_eq!(
        rendered(&mut session.rt, messages[0]),
        format!("{{tcp_closed,{port_text}}}"),
        "and it is the same 2-tuple"
    );
}

/// `{exit_on_close, false}` keeps the socket half-open: the message is delivered, the port stays
/// **live**, and the once-only latch still forbids a second message.
///
/// Measured on the oracle: with `{exit_on_close,false}` the owner gets `{tcp_closed,S}` exactly
/// once and `erlang:port_info(S, connected)` still answers the owner afterwards.
#[test]
fn the_exit_on_close_false_socket_survives_the_peer_close_message() {
    let Some((mut session, mut table, owner)) = table_owned("peer close: half open") else {
        return;
    };
    let peer = match session.connect_loopback("peer close: half open") {
        Some(peer) => peer,
        None => return,
    };
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    assert_eq!(
        session.setopts(&record(INET_LOPT_EXITONCLOSE, 0)),
        vec![1],
        "the driver accepts `{{exit_on_close,false}}`"
    );
    drop(peer);

    assert_eq!(
        session.deliver_until(&mut table, DELIVERY_BOUND),
        1,
        "the close is still reported once"
    );
    assert!(
        session.rt.ports.borrow().is_live(session.port),
        "`exitf = 0` leaves the socket half-open — nothing may retire it"
    );
    assert_eq!(
        mailbox_terms(&table, owner).len(),
        1,
        "exactly one `{{tcp_closed, S}}`, never a second message"
    );
    assert_eq!(
        session
            .rt
            .deliver_peer_closes(&mut table)
            .expect("a second delivery pass"),
        0,
        "the once-only latch is keyed on *reported*, so a live port reports nothing more"
    );
}

// -----------------------------------------------------------------------------------------------
// The oracle cross-checks: the same arm/spend/close/re-arm sequence on a real `erl`
// -----------------------------------------------------------------------------------------------

/// The `{active,2}` sequence the spent-counter case drives, as one `gen_tcp` program (a real
/// listen/accept plus a client, so the peer's close is a real EOF): arm two grants, spend both one
/// byte at a time, close the peer, make one passive read (the `TCP_REQ_RECV` the case sends as a
/// control op), re-arm `{active,true}`.
const TWO_GRANT_PROBE: &str = "begin \
     {ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true}]), \
     {ok,P} = inet:port(L), \
     {ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false}]), \
     {ok,S} = gen_tcp:accept(L), \
     ok = inet:setopts(S,[{active,2}]), \
     ok = gen_tcp:send(C,<<\"a\">>), \
     One = receive X1 -> X1 after 2000 -> timeout end, \
     ok = gen_tcp:send(C,<<\"b\">>), \
     Two = receive X2 -> X2 after 2000 -> timeout end, \
     Passive = receive X3 -> X3 after 2000 -> timeout end, \
     gen_tcp:close(C), \
     Silent = receive X4 -> X4 after 1000 -> timeout end, \
     Recv = gen_tcp:recv(S,0,500), \
     RecvAfter = receive X6 -> X6 after 500 -> timeout end, \
     ok = inet:setopts(S,[{active,true}]), \
     Closed = receive X5 -> X5 after 2000 -> timeout end, \
     io:format(\"one=~w~ntwo=~w~npassive=~w~nsilent=~w~nrecv=~w~nrecv_after=~w~nclosed=~w~n\", \
     [One,Two,Passive,Silent,Recv,RecvAfter,Closed]), \
     gen_tcp:close(S), gen_tcp:close(L) end";

/// The `{active,once}` sequence the spent-once case drives: one granted read (which owes **no**
/// `{tcp_passive, S}`), the peer's close, one passive read, then the re-arm.
const ONE_GRANT_PROBE: &str = "begin \
     {ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true}]), \
     {ok,P} = inet:port(L), \
     {ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false}]), \
     {ok,S} = gen_tcp:accept(L), \
     ok = inet:setopts(S,[{active,once}]), \
     ok = gen_tcp:send(C,<<\"x\">>), \
     Data = receive X1 -> X1 after 2000 -> timeout end, \
     Passive = receive X2 -> X2 after 2000 -> timeout end, \
     gen_tcp:close(C), \
     Silent = receive X3 -> X3 after 1000 -> timeout end, \
     Recv = gen_tcp:recv(S,0,500), \
     RecvAfter = receive X5 -> X5 after 500 -> timeout end, \
     ok = inet:setopts(S,[{active,true}]), \
     Closed = receive X4 -> X4 after 2000 -> timeout end, \
     io:format(\"data=~w~npassive=~w~nsilent=~w~nrecv=~w~nrecv_after=~w~nclosed=~w~n\", \
     [Data,Passive,Silent,Recv,RecvAfter,Closed]), \
     gen_tcp:close(S), gen_tcp:close(L) end";

/// `text` with every port term masked to `#Port<P>` — the oracle's port number is its own and
/// differs per run, so a pinned literal names the message **shape** (the rule
/// `active_input_e2e_tests.rs::mask_ports` follows).
fn mask_ports(text: &str) -> String {
    const OPEN: &str = "#Port<";
    let mut masked = String::with_capacity(text.len());
    for (index, part) in text.split(OPEN).enumerate() {
        if index == 0 {
            masked.push_str(part);
            continue;
        }
        match part.find('>') {
            Some(close) => {
                masked.push_str("#Port<P>");
                masked.push_str(&part[close + 1..]);
            }
            None => {
                masked.push_str(OPEN);
                masked.push_str(part);
            }
        }
    }
    masked
}

/// Runs one probe and answers its **masked** stdout, or `None` when no usable `erl` is on `PATH`
/// (the caller then prints [`skip_oracle_check`]'s banner).
fn masked_oracle(expr: &str) -> Option<String> {
    Some(mask_ports(&erl_eval(expr)?))
}

/// The `{active,2}` close sequence, cross-checked against a real `erl`: the two granted reads are
/// two messages, the exhaust sends one `{tcp_passive, S}`, the close afterwards is **silent**
/// (`tcp_recv_closed`'s passive arm, `inet_drv.c:12819-12832`) — a passive read is what observes it
/// and answers `{error,closed}` without answering a message — and re-arming `{active,true}` is what
/// produces the single `{tcp_closed, S}` (`inet_set_opts`'s sticky re-fire, `:7001-7015`).
#[test]
fn the_oracle_agrees_that_an_exhausted_active_n_grant_silences_the_close() {
    let Some(printed) = masked_oracle(TWO_GRANT_PROBE) else {
        skip_oracle_check("the spent `{active,N}` close");
        return;
    };
    assert_eq!(
        printed,
        "one={tcp,#Port<P>,<<97>>}\n\
         two={tcp,#Port<P>,<<98>>}\n\
         passive={tcp_passive,#Port<P>}\n\
         silent=timeout\n\
         recv={error,closed}\n\
         recv_after=timeout\n\
         closed={tcp_closed,#Port<P>}",
        "the oracle's own answer for the `{{active,2}}` spend/close/recv/re-arm sequence"
    );
}

/// The `{active,once}` twin, cross-checked against the same oracle: one granted read, **no**
/// `{tcp_passive, S}` (the C sends it only for `INET_MULTI`, `inet_drv.c:1556-1558`), a silent
/// close whose EOF the passive read is what discovers, and the one report the re-arm owes.
#[test]
fn the_oracle_agrees_that_a_spent_active_once_grant_silences_the_close() {
    let Some(printed) = masked_oracle(ONE_GRANT_PROBE) else {
        skip_oracle_check("the spent `{active,once}` close");
        return;
    };
    assert_eq!(
        printed,
        "data={tcp,#Port<P>,<<120>>}\n\
         passive=timeout\n\
         silent=timeout\n\
         recv={error,closed}\n\
         recv_after=timeout\n\
         closed={tcp_closed,#Port<P>}",
        "the oracle's own answer for the `{{active,once}}` spend/close/recv/re-arm sequence"
    );
}
