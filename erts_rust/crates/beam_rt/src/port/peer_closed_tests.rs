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
//! Four of the six cases are pure: the `{active, …}`/`{exit_on_close, …}` mirror reads, the
//! once-only latch with its recycle rule, and the driver-kind gate. Two drive **real loopback
//! traffic** through a real `{spawn_driver, "tcp_inet"}` / `{spawn_driver, "udp_inet"}` port opened
//! by the runtime's own `open_port/2` hook, close the peer, and read the latch back through
//! [`PortRegistry::drain_peer_closes`] — the two halves of the invariant that matter most
//! (invariant 1: once, and only on an active socket; invariant 6: never for a datagram port).
//!
//! # Loud skips, never a silent pass
//!
//! A sandbox that refuses `socket(2)`/`bind(2)` cannot host a real-traffic case: [`skip_no_loopback`]
//! prints a banner and the case returns. No case depends on another having run.
//!
//! # The oracle these cases reproduce (OTP 29.1.1 / erts-17.1, measured)
//!
//! * `tcp_recv_closed` (`erts/emulator/drivers/common/inet_drv.c:12783-12845`): an **active**
//!   socket sends one `{tcp_closed, S}` (2-tuple) and dies when `exit_on_close` (the default);
//!   a **passive** one sends nothing (`:12819-12832`).
//! * `tcp_closed_message`'s `TCP_ADDF_CLOSE_SENT` guard (`:4064-4071`): at most once per
//!   descriptor, however often the driver observes the EOF.
//! * `inet_set_opts`'s `INET_LOPT_ACTIVE` arm (`:6968-7016`): a passive descriptor already closed
//!   sends the message **at that moment** when an active mode is armed — the sticky re-fire.
//! * `rg 'udp_closed' erts/` has **zero** hits: a `udp_inet` port has no such message.

use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_bifs::PortHook;
use beam_engine::terms;
use beam_process::PortId;

use super::{PeerCloseKind, PeerCloseReport};
use crate::config::RuntimeConfig;
use crate::port::ops::inet_opts::{is_setopts_control, setopts_succeeded};
use crate::port::tests::{fixture, Fixture};
use crate::port::{RuntimeContext, RuntimePorts};
use crate::Runtime;

// -----------------------------------------------------------------------------------------------
// The wire vocabulary (the option codes and request numbers the C's `inet` driver parses)
// -----------------------------------------------------------------------------------------------

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the Erlang
/// side adds to every `inet` control request.
const INET_CONTROL_MAGIC: u64 = 0x03f1_a300;
/// `INET_REQ_SETOPTS` (7) — the request `prim_inet:setopts/2` sends for a record list.
const INET_REQ_SETOPTS: u64 = 7;
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
/// `INET_PASSIVE` (0) — `{active, false}`, the driver's default.
const INET_PASSIVE: i32 = 0;
/// How long a case waits for a report it **expects**: a fan-out bound, never a sleep.
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
            session.control("open", &[INET_AF_INET, INET_SOCK_STREAM, INET_PROTO_DEFAULT]),
            vec![1],
            "{label}: INET_REQ_OPEN on a stream/inet port answers the bare `[1]`"
        );
        assert_eq!(
            session.control("connect", &loopback_address(address.port())),
            vec![1],
            "{label}: a completed loopback connect answers the bare `[1]`"
        );
        let peer = match listener.accept() {
            Ok((peer, _)) => peer,
            Err(error) => {
                skip_no_loopback(&format!("{label}: the loopback accept failed: {error}"));
                return None;
            }
        };
        Some((session, peer))
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
    /// `note_inet_setopts`); the explicit call below only runs when the hook recorded nothing, so
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

    /// How many `setopts` records the port's mirror holds.
    fn recorded(&self) -> usize {
        let port = self.port;
        self.rt
            .ports
            .borrow()
            .inet_options(port)
            .map_or(0, |options| options.records().len())
    }

    /// One pump round, then one read of the peer-close channel — the two steps a consuming route
    /// performs, in the order `src/port.rs` performs them.
    fn round(&mut self) -> Vec<PeerCloseReport> {
        let _ = {
            let mut registry = self.rt.ports.borrow_mut();
            registry
                .pump(PUMP_SLICE)
                .unwrap_or_else(|error| panic!("the port round: {error}"))
        };
        self.rt.ports.borrow_mut().drain_peer_closes()
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
