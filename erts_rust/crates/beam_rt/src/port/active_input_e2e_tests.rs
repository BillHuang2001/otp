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

//! The **`{active, …}` input channel end to end**, over **real loopback traffic** (contract A.4).
//!
//! [`super`]'s unit matrix (`active_input_tests.rs`) feeds the renderers made-up reads: it pins the
//! term shapes but proves nothing about what a real socket driver reports. This suite is the
//! opposite half. Every case here
//!
//! 1. opens a real `{spawn_driver, "tcp_inet"}` / `{spawn_driver, "udp_inet"}` port through the
//!    runtime's own `open_port/2` hook,
//! 2. really connects it to a `std::net` peer (or really binds it and really sends it a datagram),
//! 3. arms it with the C's own numeric/`setopts` control ops,
//! 4. pumps the port layer until the delivery arrives, and
//! 5. renders the message and compares it with the **oracle's own printed rendering** for the same
//!    sequence (pinned by [`the_oracle_renders_every_shape_these_cases_assert`]).
//!
//! # The delivery seam this suite stops at
//!
//! [`PortRegistry::drain_active_inputs`](crate::port::PortRegistry) hands out
//! [`ActiveInputPacket`]s, **not** `PortEvent`s: wrapping a packet in an event, publishing it in
//! `pending_events` and delivering it into `packet.owner`'s mailbox is `src/port.rs`'s (the port
//! provider's) job, and this suite deliberately does not touch it. So each case here does exactly
//! what the provider's delivery arm will do and nothing more:
//!
//! ```text
//! pump the port round  ->  drain_active_inputs()  ->  RuntimeContext::active_input_message(&packet)
//! ```
//!
//! The evidence below therefore covers the **whole channel up to the message term** — the driver's
//! state machine, the staged [`beam_io::ActiveInput`], the drain-time owner/mode/kind binding, and
//! every byte of the rendering — and stops one step short of the mailbox write.
//!
//! # The `{mode, …}` seam, and the residual this suite records
//!
//! [`PortMeta::data_mode`](crate::port::PortMeta) returns the port's **open-time** stream option,
//! and `prim_inet:open/6` always opens the driver port `[binary]`
//! (`erts/preloaded/src/prim_inet.erl:99-137`). A `{mode, list}` socket is therefore a `[binary]`
//! port whose **driver** holds `INET_MODE_LIST`, and the delivery arm has to resolve the effective
//! mode through [`PortRegistry::data_mode`](crate::port::PortRegistry) (the `{mode, …}` record the
//! driver accepted, `src/port/inet_opts.rs`), not through the open-time option. This suite asserts
//! both halves of that: the drained packet's `data_mode` is still `Binary`, and the resolved mode
//! is `List` and renders a char list — the oracle's own answer for the same socket.
//!
//! # The `note_inet_setopts` hook (documented stand-in)
//!
//! [`PortRegistry::port_control`](crate::port::PortRegistry) is where a real `setopts` becomes part
//! of the mode mirror. That one-line call is **not wired yet** in `src/port.rs` (owned by another
//! agent), so every `setopts` below is followed by an explicit
//! [`PortRegistry::note_inet_setopts`](crate::port::PortRegistry) with the reply's own success tag
//! — the same call, from the same reply, that the hook will make (this is the pattern
//! `accept_inherit_tests.rs` established). When the hook lands these explicit calls become
//! redundant, not wrong.
//!
//! # The oracle, measured (OTP 29.1.1 / erts-17.1)
//!
//! Every literal asserted below is the system `erl`'s own `~w` rendering for the same real-traffic
//! sequence; the port term is masked by comparing against this runtime's own `S` (the assertions
//! build the expected text with [`Session::port_text`], and each one quotes the oracle's line in its
//! message so a future reader can re-check it):
//!
//! | arming | client write | oracle |
//! |---|---|---|
//! | `{active,once}`, `[binary]` | `<<"hello">>` | `{tcp,#Port<0.5>,<<104,101,108,108,111>>}` |
//! | the grant is spent, no re-arm | `<<"world">>` | *nothing* |
//! | re-armed, then written | `<<"again">>` | `{tcp,#Port<0.5>,<<119,111,114,108,100>>}` (the passive window's `<<"world">>`) |
//! | `{mode,list}` | `<<"hello">>` | `{tcp,#Port<0.5>,[104,101,108,108,111]}`, `is_list(Data)` = true (the probe re-arms after the passive window, so its own `[mode,list]` line carries those buffered bytes) |
//! | `{packet,2}` | `<<0,5,"hello">>` | `{tcp,#Port<0.5>,<<104,101,108,108,111>>}` (header stripped) |
//! | `{header,2}` | `<<"hello">>` | `{tcp,#Port<0.5>,[104,101\|<<108,108,111>>]}` |
//! | `{active,2}` | `<<"one">>`, `<<"two">>`, `<<"three">>` | `{tcp,…<<"one">>}`, `{tcp,…<<"two">>}`, `{tcp_passive,#Port<0.5>}`, then nothing |
//! | `{active,false}` / never armed | `<<"hi">>` | *nothing* |
//! | UDP `{active,once}` | `<<"hi">>` / `<<>>` | `{udp,#Port<0.3>,{127,0,0,1},56678,<<104,105>>}` / `…,<<>>}` |
//! | UDP `{mode,list}` | `<<"hi">>` | `{udp,#Port<0.3>,{127,0,0,1},56678,[104,105]}` |
//!
//! # Two measured differences from the oracle (both recorded, neither hidden)
//!
//! * **The passive window's bytes.** On the oracle a write that arrives while an
//!   `{active,once}`/`{active,N}` grant is spent stays in the socket (the C calls
//!   `sock_select(desc, FD_READ, 0)`), so the *re-arm's* inline read delivers those bytes. Here the
//!   poll registration stays armed (`beam_io`'s poller is edge-triggered and has no empty interest
//!   mask), so `async_ready_input` harvests them into the **port data queue** instead and the
//!   re-arm's inline burst finds the socket empty. [`an_active_once_tcp_delivery_is_one_binary_and_the_next_write_waits_for_a_rearm`]
//!   therefore asserts what both sides agree on — a write under a spent grant delivers nothing, and
//!   the next write after a re-arm is delivered — and writes its third payload itself rather than
//!   relying on the passive window's bytes.
//! * **`hsz > len`.** `beam_io`'s `split_payload` clamps the `{header, N}` size to the payload, so a
//!   header wider than the payload reaches the renderer as a whole-payload header with a
//!   present-but-empty body; the C's `(mode == LIST) || (hsz > len)` branch would render one flat
//!   string instead. That is a `beam_io` seam (`drivers/inet/active.rs` does not carry the raw
//!   `hsz`), not fixable here, and it is pinned at the renderer level by
//!   `a_header_as_wide_as_the_payload_keeps_an_empty_binary_tail` in `active_input_tests.rs`; every
//!   shape this suite exercises has `hsz <= len`, so it is not observable over the wire.
//!
//! # Loud skips, never a silent pass
//!
//! A sandbox that refuses `socket(2)`/`bind(2)`, or a host with no usable `erl`, cannot host a
//! real-traffic case: [`skip_no_loopback`] / [`skip_oracle_check`] print a banner and the case
//! returns. Every assertion stands on its own — no case depends on another having run.

use std::io::Write;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_bifs::PortHook;
use beam_engine::terms;
use beam_io::ActiveInputEvent;
use beam_process::PortId;
use beam_term::{make_small, Eterm};

use super::ActiveInputPacket;
use crate::config::RuntimeConfig;
use crate::port::ops::inet_opts::{is_setopts_control, setopts_succeeded};
use crate::port::tests::{fixture, skip_oracle_check, Fixture};
use crate::port::{RuntimeContext, RuntimePorts, StreamType};
use crate::Runtime;

// -----------------------------------------------------------------------------------------------
// The wire vocabulary (`inet_drv.c`'s request numbers, option codes and operand tags)
// -----------------------------------------------------------------------------------------------

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the Erlang
/// side adds to every `inet` control request, which both `*_inet_ctl` entries subtract again.
const INET_CONTROL_MAGIC: u64 = 0x03f1_a300;
/// `INET_REQ_SETOPTS` (7) — the request `prim_inet:setopts/2` sends for a record list.
const INET_REQ_SETOPTS: u64 = 7;
/// `INET_LOPT_HEADER` (21) — `desc->inet.hsz`, the packet header size.
const INET_LOPT_HEADER: u8 = 21;
/// `INET_LOPT_ACTIVE` (22) — `desc->inet.active` plus, in the multi form, a 16-bit counter.
const INET_LOPT_ACTIVE: u8 = 22;
/// `INET_LOPT_PACKET` (23) — `desc->inet.htype`, the C's `TCP_PB_*` code (`{packet,2}` ⇒ `2`).
const INET_LOPT_PACKET: u8 = 23;
/// `INET_LOPT_MODE` (24) — `desc->inet.mode` (`INET_MODE_LIST` `0`, `INET_MODE_BINARY` `1`).
const INET_LOPT_MODE: u8 = 24;
/// `INET_PASSIVE` (0) — `{active, false}`, the driver's default.
const INET_PASSIVE: i32 = 0;
/// `INET_ONCE` (2) — `{active, once}`.
const INET_ONCE: i32 = 2;
/// `INET_MULTI` (3) — `{active, N}`; the record then carries a trailing 16-bit counter.
const INET_MULTI: i32 = 3;
/// `INET_MODE_LIST` (0) — `{mode, list}` (`inet_drv.c:786`).
const INET_MODE_LIST: i32 = 0;
/// `TCP_PB_2` (2) — `{packet, 2}`: a two-byte big-endian length header (`inet_drv.c:781`).
const TCP_PB_2: i32 = 2;
/// `INET_AF_INET` (1) — the tag of an explicit IPv4 address in a packed-address record.
const INET_AF_INET: u8 = 1;
/// `INET_AF_LOOPBACK` (4) — the tag of the loopback shorthand (`[4, port_hi, port_lo]`).
const INET_AF_LOOPBACK: u8 = 4;
/// `SOCK_STREAM` (1) — the `INET_REQ_OPEN` operand of a `tcp_inet` socket.
const INET_SOCK_STREAM: u8 = 1;
/// `SOCK_DGRAM` (2) — the `INET_REQ_OPEN` operand of a `udp_inet` socket.
const INET_SOCK_DGRAM: u8 = 2;
/// The C's "default protocol for this socket type" (0) — TCP for a stream, UDP for a datagram.
const INET_PROTO_DEFAULT: u8 = 0;
/// How long a case waits for a delivery it **expects**: a fan-out bound, never a sleep.
const DELIVERY_BOUND: Duration = Duration::from_secs(5);
/// How long a case polls to prove something is **not** delivered — six `PORT_POLL_SLICE` rounds,
/// each of which blocks on a real `epoll_wait`, so a delivery that exists has every chance to
/// arrive.
const QUIET_WINDOW: Duration = Duration::from_millis(300);
/// One pump round's slice — the port poll budget a zero-slice scheduler pass hands this layer.
const PUMP_SLICE: Duration = Duration::from_millis(50);

// -----------------------------------------------------------------------------------------------
// The wire helpers (the C's operand encodings)
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

/// The C's packed IPv4 address record `[INET_AF_INET, port_hi, port_lo, 127, 0, 0, 1]` — the
/// operand of a `connect` to a loopback peer.
fn loopback_address(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_INET, hi, lo, 127, 0, 0, 1]
}

// -----------------------------------------------------------------------------------------------
// Term helpers
// -----------------------------------------------------------------------------------------------

/// Element `index` (0-based) of a tuple the channel just built.
fn element(term: Eterm, index: usize) -> Eterm {
    terms::tuple_element(term, index)
        .unwrap_or_else(|error| panic!("element {index} of a built message: {error}"))
}

/// The arity of a tuple the channel just built.
fn arity(term: Eterm) -> usize {
    terms::tuple_arity(term).unwrap_or_else(|error| panic!("the arity of a built message: {error}"))
}

/// `term` read as a binary's bytes.
fn binary(term: Eterm) -> Vec<u8> {
    terms::binary_bytes(term)
        .unwrap_or_else(|error| panic!("a `[binary]` delivery's payload: {error}"))
        .to_vec()
}

/// `term` read as a proper list's elements.
fn list(term: Eterm) -> Vec<Eterm> {
    terms::list_elements(term).unwrap_or_else(|error| panic!("a char list payload: {error}"))
}

// -----------------------------------------------------------------------------------------------
// The session: the runtime, its port provider, and the one port under test
// -----------------------------------------------------------------------------------------------

/// A live [`Runtime`] plus the port provider built over the runtime's **own** registry — so the port
/// opened here is the port the runtime's `port_control` path and its `pump` see.
///
/// Mirrors `inet_matrix_tests.rs`'s `Session`: the provider must own the runtime's registry, not the
/// fixture's, or the drain and the pump would look at different tables.
struct Session {
    /// The runtime whose `ports` registry every port is opened on, and whose `-eval` arena every
    /// message is built on.
    rt: Runtime,
    /// The term/heap fixture (the `open_port/2` `Name`/`Options` vocabulary).
    fx: Fixture,
    /// The provider under test.
    hook: RuntimePorts,
    /// The single port every helper below drives.
    port: PortId,
}

impl Session {
    /// A runtime and its provider **with no port open yet**; the constructors below open one and
    /// assign it to [`Session::port`].
    fn bare() -> Session {
        let fx = fixture();
        let rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let owner = rt.self_pid();
        let hook = fx.hook_over(Rc::clone(&rt.ports), owner);
        Session {
            rt,
            fx,
            hook,
            // A placeholder: `port` is assigned by whichever constructor opens the socket, and
            // nothing below runs before that.
            port: PortId::new(0),
        }
    }

    /// A `tcp_inet` port opened, `open`(2)'d and **connected** to a real loopback listener, plus the
    /// accepted `std::net` peer it is connected to.
    ///
    /// `None` when the environment cannot host the case ([`skip_no_loopback`] has then printed the
    /// banner), never a silent pass.
    fn connected_tcp(label: &str) -> Option<(Session, TcpStream)> {
        let mut session = Session::bare();
        let port = session.open("tcp_inet", label)?;
        session.port = port;
        let listener = match TcpListener::bind("127.0.0.1:0") {
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
        // The C's `INET_REQ_CONNECT` in this driver's **named** form (which takes the address with
        // no `Timeout(4)` prefix): a loopback connect completes inside the call and answers `[1]`.
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
        assert!(
            peer.set_write_timeout(Some(Duration::from_secs(5))).is_ok(),
            "{label}: the peer takes a write timeout"
        );
        Some((session, peer))
    }

    /// A `udp_inet` port opened and **bound** to a kernel-assigned loopback port, plus that port
    /// number (the address a real `UdpSocket` sends to).
    fn bound_udp(label: &str) -> Option<(Session, u16)> {
        let mut session = Session::bare();
        let port = session.open("udp_inet", label)?;
        session.port = port;
        assert_eq!(
            session.control("open", &[INET_AF_INET, INET_SOCK_DGRAM, INET_PROTO_DEFAULT]),
            vec![1],
            "{label}: INET_REQ_OPEN on a datagram/inet port answers the bare `[1]`"
        );
        let bound = session.control("bind", &[INET_AF_LOOPBACK, 0, 0]);
        assert_eq!(
            bound.first(),
            Some(&1),
            "{label}: INET_REQ_BIND answers `[1] ++ <<Port:16/big>>`: {bound:?}"
        );
        assert_eq!(bound.len(), 3, "{label}: bind reports the port: {bound:?}");
        let number = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(number, 0, "{label}: the kernel assigned a real port");
        Some((session, number))
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

    /// Runs one control op (a named verb or the decimal numeric request) on the port under test and
    /// fails loudly with the driver's own error text.
    fn control(&mut self, verb: &str, arg: &[u8]) -> Vec<u8> {
        let port = self.port;
        self.rt
            .ports
            .borrow_mut()
            .port_control(port, verb, arg)
            .unwrap_or_else(|error| panic!("the {verb:?} control op on {port:?}: {error}"))
    }

    /// Applies a `setopts` record list through the numeric request and mirrors the reply into the
    /// port's option table — the documented stand-in for the (not yet wired, `src/port.rs`-owned)
    /// `note_inet_setopts` hook. Returns the driver's reply.
    fn setopts(&mut self, arg: &[u8]) -> Vec<u8> {
        let reply = self.control(&setopts_op(), arg);
        let ok = setopts_succeeded(&reply);
        let port = self.port;
        self.rt.ports.borrow_mut().note_inet_setopts(port, arg, ok);
        reply
    }

    /// One pump round followed by a drain of the active-input channel — the two steps a consuming
    /// route performs, in the order `src/port.rs` performs them.
    fn round(&mut self) -> Vec<ActiveInputPacket> {
        {
            let mut registry = self.rt.ports.borrow_mut();
            let _events = registry
                .pump(PUMP_SLICE)
                .unwrap_or_else(|error| panic!("the port round: {error}"));
        }
        self.rt.ports.borrow_mut().drain_active_inputs()
    }

    /// Pumps until the channel produces a delivery or `bound` expires, returning **everything** the
    /// producing round staged (one driver callback can stage a data message *and* the passive
    /// message that followed it).
    fn collect(&mut self, bound: Duration) -> Vec<ActiveInputPacket> {
        let deadline = Instant::now() + bound;
        loop {
            let packets = self.round();
            if !packets.is_empty() || Instant::now() >= deadline {
                return packets;
            }
        }
    }

    /// Polls for [`QUIET_WINDOW`] and asserts the channel produced **nothing**, quoting the oracle's
    /// own answer for the same sequence.
    fn assert_silent(&mut self, what: &str, oracle: &str) {
        let deadline = Instant::now() + QUIET_WINDOW;
        let mut packets = Vec::new();
        loop {
            packets.extend(self.round());
            if Instant::now() >= deadline {
                break;
            }
        }
        assert!(
            packets.is_empty(),
            "{what}: the oracle delivers nothing here ({oracle}), this channel staged {packets:?}"
        );
    }

    /// `packet` re-bound to the port's **effective** data mode — the `{mode, …}` record the driver
    /// accepted ([`PortRegistry::data_mode`](crate::port::PortRegistry)), falling back to the
    /// open-time stream. This is the resolution the provider's delivery arm must perform; see the
    /// module docs.
    fn resolved(&self, packet: &ActiveInputPacket) -> ActiveInputPacket {
        let mode = self
            .rt
            .ports
            .borrow()
            .data_mode(packet.port, packet.data_mode);
        ActiveInputPacket {
            port: packet.port,
            owner: packet.owner,
            data_mode: mode,
            kind: packet.kind,
            event: packet.event.clone(),
        }
    }

    /// The message [`RuntimeContext::active_input_message`] builds for `packet`, with the port's
    /// effective data mode applied.
    fn message(&mut self, packet: &ActiveInputPacket) -> Eterm {
        let resolved = self.resolved(packet);
        let mut context = RuntimeContext::new(&mut self.rt);
        context
            .active_input_message(&resolved)
            .unwrap_or_else(|error| panic!("an active-input message for {resolved:?}: {error}"))
    }

    /// `packet`'s message rendered by the registered `~w` printer — the exact text the oracle's own
    /// `io:format("~w",[Message])` prints.
    fn render(&mut self, packet: &ActiveInputPacket) -> String {
        let message = self.message(packet);
        self.print(message)
    }

    /// `term` rendered by the registered `~w` printer.
    fn print(&mut self, term: Eterm) -> String {
        self.rt
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a built term: {error}"))
            .trim_end_matches('\n')
            .to_string()
    }

    /// This session's port term rendered the way the oracle prints it (`#Port<0.N>`) — the `S`
    /// element's spelling, which no assertion here invents.
    fn port_text(&mut self) -> String {
        let term = self.port.to_eterm();
        self.print(term)
    }

    /// The atom `name` of `term`, for an assertion that reads as the atom the C sends.
    fn atom_name(&self, term: Eterm) -> Option<String> {
        self.rt.atom_name_of(term).map(str::to_string)
    }

    /// Assert `term` is the atom `name`.
    fn assert_atom(&self, what: &str, term: Eterm, name: &str) {
        assert_eq!(
            self.atom_name(term).as_deref(),
            Some(name),
            "{what} must be the atom `{name}`"
        );
    }

    /// Assert a message's `S` element is this session's port term — never a fabricated atom.
    fn assert_port(&self, what: &str, message: Eterm) {
        assert_eq!(
            element(message, 1),
            self.port.to_eterm(),
            "{what}: the `S` element is the port's own term (`PortId::to_eterm`)"
        );
    }

    /// The `Data` of a `{tcp, S, Data}` message built for `packet`, with the tag and port checked.
    fn tcp_data(&mut self, packet: &ActiveInputPacket) -> Eterm {
        let message = self.message(packet);
        assert_eq!(arity(message), 3, "{{tcp, S, Data}} is a 3-tuple");
        self.assert_atom("the tag", element(message, 0), "tcp");
        self.assert_port("a `{{tcp, S, Data}}` message", message);
        element(message, 2)
    }
}

/// The loud skip banner for a case the **environment** cannot host: no loopback socket at all.
/// Never a silent pass, never a panic.
fn skip_no_loopback(what: &str) {
    println!("================================================================");
    println!("SKIP: {what}");
    println!("      (this case needs real loopback traffic and could not get it)");
    println!("================================================================");
}

// -----------------------------------------------------------------------------------------------
// Test 1 — `{active, once}`, binary mode, over real TCP traffic
// -----------------------------------------------------------------------------------------------

/// An `{active, once}` `[binary]` socket delivers exactly **one** `{tcp, S, <<Data>>}` per grant:
/// the write under the grant arrives, a write while the grant is spent delivers nothing at all, and
/// the next write after a re-arm is delivered again.
///
/// The oracle's own sequence (`gen_tcp:listen` + a real loopback client + a re-armed `inet:setopts`)
/// prints `{tcp,#Port<0.5>,<<104,101,108,108,111>>}` for `<<"hello">>`, `timeout` for the write
/// under the spent grant, and a `{tcp, …}` again once re-armed — see the module docs' table and
/// [`the_oracle_renders_every_shape_these_cases_assert`].
#[test]
fn an_active_once_tcp_delivery_is_one_binary_and_the_next_write_waits_for_a_rearm() {
    let Some((mut session, mut peer)) =
        Session::connected_tcp("active input: {active,once} over TCP")
    else {
        return;
    };

    // (a) Arm, then write: one delivery, the bare payload binary.
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    peer.write_all(b"hello")
        .expect("the loopback peer writes `hello`");
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(
        packets.len(),
        1,
        "one armed write is exactly one delivery, staged {packets:?}"
    );
    let packet = &packets[0];
    assert_eq!(
        packet.port,
        session.port,
        "the delivery names the port that produced it"
    );
    assert!(
        matches!(&packet.event, ActiveInputEvent::Data(read) if read.header.is_empty() && read.body.as_deref() == Some(&b"hello"[..])),
        "`{{packet,0}}` hands the whole write up as the body, with no `{{header,N}}` prefix: {:?}",
        packet.event
    );
    let data = session.tcp_data(packet);
    assert_eq!(
        binary(data),
        b"hello",
        "a `[binary]` delivery is one binary of the payload bytes"
    );
    assert_eq!(
        session.render(packet),
        format!("{{tcp,{},<<104,101,108,108,111>>}}", session.port_text()),
        "the oracle prints `{{tcp,#Port<0.5>,<<104,101,108,108,111>>}}` for \
         `gen_tcp:send(C,<<\"hello\">>)` on an `{{active,once}}` `[binary]` socket"
    );

    // (b) The grant is spent: a second write delivers nothing on this channel.
    peer.write_all(b"world")
        .expect("the loopback peer writes `world`");
    session.assert_silent(
        "a write on a spent `{active,once}` socket",
        "`m2 = timeout` — the oracle's second receive times out",
    );

    // (c) Re-armed, the next write is delivered again.
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts the re-arm"
    );
    peer.write_all(b"again")
        .expect("the loopback peer writes `again`");
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(
        packets.len(),
        1,
        "a re-armed socket delivers the next write once, staged {packets:?}"
    );
    assert_eq!(
        binary(session.tcp_data(&packets[0])),
        b"again",
        "the re-arm's delivery is the payload written under it"
    );
}

// -----------------------------------------------------------------------------------------------
// Test 2 — `{mode, list}`
// -----------------------------------------------------------------------------------------------

/// A `{mode, list}` `setopts` turns the very same delivery into a **charlist**, even though the
/// port was opened `[binary]` (`prim_inet:open/6` always does) and its metadata still says so.
///
/// This is the `data_mode` seam, asserted from both ends: the drained packet's `data_mode` is
/// [`StreamType::Binary`] (what [`PortMeta::data_mode`](crate::port::PortMeta) answers — the
/// open-time option), while the port's **effective** mode resolved through
/// [`PortRegistry::data_mode`](crate::port::PortRegistry) is [`StreamType::List`], because the
/// driver really accepted the `{mode,list}` record.
///
/// Measured on the oracle: `{tcp,#Port<0.5>,[104,101,108,108,111]}` with `is_list(Data) = true`.
#[test]
fn a_mode_list_active_delivery_renders_the_same_payload_as_a_charlist() {
    let Some((mut session, mut peer)) =
        Session::connected_tcp("active input: {mode,list}")
    else {
        return;
    };

    assert_eq!(
        session.setopts(&record(INET_LOPT_MODE, INET_MODE_LIST)),
        vec![1],
        "the driver accepts `{{mode,list}}` (INET_MODE_LIST is 0)"
    );
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    peer.write_all(b"hello")
        .expect("the loopback peer writes `hello`");
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(packets.len(), 1, "one delivery, staged {packets:?}");
    let packet = &packets[0];

    assert_eq!(
        packet.data_mode,
        StreamType::Binary,
        "the drained packet's mode is the port's OPEN-TIME stream (`[binary]`, as `prim_inet:open/6` \
         opens it) — `PortMeta::data_mode` does not consult the `{{mode,…}}` mirror yet"
    );
    assert_eq!(
        session
            .rt
            .ports
            .borrow()
            .data_mode(packet.port, packet.data_mode),
        StreamType::List,
        "the effective mode is the `{{mode,list}}` record the driver accepted: this is the seam the \
         delivery arm must resolve through, and what the renderer below is given"
    );

    let data = session.tcp_data(packet);
    assert_eq!(
        list(data),
        b"hello".iter().map(|byte| make_small(i64::from(*byte))).collect::<Vec<_>>(),
        "a `[list]` delivery is one flat char list, one element per payload byte"
    );
    assert_eq!(
        session.render(packet),
        format!("{{tcp,{},[104,101,108,108,111]}}", session.port_text()),
        "the oracle prints `{{tcp,#Port<0.5>,[104,101,108,108,111]}}` (with `is_list(Data)` true) \
         for `{{mode,list}}` over `<<\"hello\">>`"
    );
}

// -----------------------------------------------------------------------------------------------
// Test 3 — `{packet, 2}`
// -----------------------------------------------------------------------------------------------

/// Under `{packet, 2}` the delivered binary is the **payload alone**: the two-byte length header is
/// consumed by the frame decoder and never reaches the owner.
///
/// The client writes a real framed message (`<<0,5,"hello">>`); the oracle delivers
/// `{tcp,#Port<0.5>,<<104,101,108,108,111>>}` — five bytes, not seven.
#[test]
fn a_packet_two_delivery_has_its_framing_header_stripped() {
    let Some((mut session, mut peer)) =
        Session::connected_tcp("active input: {packet,2}")
    else {
        return;
    };

    assert_eq!(
        session.setopts(&record(INET_LOPT_PACKET, TCP_PB_2)),
        vec![1],
        "the driver accepts `{{packet,2}}`"
    );
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    peer.write_all(&[0, 5, b'h', b'e', b'l', b'l', b'o'])
        .expect("the loopback peer writes one framed message");
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(packets.len(), 1, "one frame is one delivery, staged {packets:?}");
    let packet = &packets[0];
    assert!(
        matches!(&packet.event, ActiveInputEvent::Data(read) if read.header.is_empty()),
        "the `{{packet,2}}` framing header is stripped before the read is handed up: {:?}",
        packet.event
    );
    assert_eq!(
        binary(session.tcp_data(packet)),
        b"hello",
        "the delivered payload is the five body bytes, not the seven written bytes"
    );
    assert_eq!(
        session.render(packet),
        format!("{{tcp,{},<<104,101,108,108,111>>}}", session.port_text()),
        "the oracle prints `{{tcp,#Port<0.5>,<<104,101,108,108,111>>}}` for a `{{packet,2}}` \
         `<<0,5,\"hello\">>` write"
    );
}

// -----------------------------------------------------------------------------------------------
// Test 4 — `{header, 2}`
// -----------------------------------------------------------------------------------------------

/// Under `{header, 2}` the first two payload bytes are **consed in front** of the rest as an
/// improper list: `[H1, H2 | <<Rest>>]` — the C's `LOAD_STRING_CONS`
/// (`inet_drv.c:3990-4005`).
///
/// Measured on the oracle: `{tcp,#Port<0.5>,[104,101|<<108,108,111>>]}` for `<<"hello">>`.
#[test]
fn a_header_two_delivery_conses_the_header_bytes_in_front_of_the_payload() {
    let Some((mut session, mut peer)) =
        Session::connected_tcp("active input: {header,2}")
    else {
        return;
    };

    assert_eq!(
        session.setopts(&record(INET_LOPT_HEADER, 2)),
        vec![1],
        "the driver accepts `{{header,2}}`"
    );
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    peer.write_all(b"hello")
        .expect("the loopback peer writes `hello`");
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(packets.len(), 1, "one delivery, staged {packets:?}");
    let packet = &packets[0];
    assert!(
        matches!(&packet.event, ActiveInputEvent::Data(read) if read.header == b"he" && read.body.as_deref() == Some(&b"llo"[..])),
        "the `{{header,2}}` split is `hsz` bytes of header and the rest as the body: {:?}",
        packet.event
    );
    assert_eq!(
        session.render(packet),
        format!("{{tcp,{},[104,101|<<108,108,111>>]}}", session.port_text()),
        "the oracle prints `{{tcp,#Port<0.5>,[104,101|<<108,108,111>>]}}` for `{{header,2}}` over \
         `<<\"hello\">>`"
    );
}

// -----------------------------------------------------------------------------------------------
// Test 5 — `{active, N}`
// -----------------------------------------------------------------------------------------------

/// An `{active, 2}` counter delivers exactly two `{tcp, S, Data}` messages, then **one**
/// `{tcp_passive, S}`, and then nothing — and a later `{active, false}` owes no second passive
/// message.
///
/// The passive message follows the delivery that spent the last credit (`tcp_reply_data`'s
/// `INET_CHECK_ACTIVE_TO_PASSIVE`, `inet_drv.c:4302`), which is why it lands in the **same** drain
/// as the second payload. Measured on the oracle:
/// `{tcp,…<<"one">>}`, `{tcp,…<<"two">>}`, `{tcp_passive,#Port<0.5>}`, then `timeout`.
#[test]
fn an_active_n_counter_delivers_n_then_exactly_one_passive_message() {
    let Some((mut session, mut peer)) =
        Session::connected_tcp("active input: {active,2}")
    else {
        return;
    };

    assert_eq!(
        session.setopts(&active_counter_record(2)),
        vec![1],
        "the driver accepts `{{active,2}}`"
    );

    peer.write_all(b"one")
        .expect("the loopback peer writes `one`");
    let first = session.collect(DELIVERY_BOUND);
    assert_eq!(
        first.len(),
        1,
        "the first write spends one of the two credits, staged {first:?}"
    );
    assert_eq!(
        binary(session.tcp_data(&first[0])),
        b"one",
        "the first delivery is the first write"
    );

    peer.write_all(b"two")
        .expect("the loopback peer writes `two`");
    let second = session.collect(DELIVERY_BOUND);
    assert_eq!(
        second.len(),
        2,
        "the second write delivers its payload AND the passive message of the spent counter, \
         staged {second:?}"
    );
    assert_eq!(
        binary(session.tcp_data(&second[0])),
        b"two",
        "the payload comes first"
    );
    assert!(
        matches!(second[1].event, ActiveInputEvent::Passive),
        "the second event is the passive transition, got {:?}",
        second[1].event
    );
    assert_eq!(
        session.render(&second[1]),
        format!("{{tcp_passive,{}}}", session.port_text()),
        "the oracle prints `{{tcp_passive,#Port<0.5>}}` once the `{{active,2}}` counter is spent"
    );

    peer.write_all(b"three")
        .expect("the loopback peer writes `three`");
    session.assert_silent(
        "a write after the `{active,2}` counter was spent",
        "`m4 = timeout` — the oracle's fourth receive times out",
    );

    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_PASSIVE)),
        vec![1],
        "the driver accepts `{{active,false}}`"
    );
    session.assert_silent(
        "a further `{active,false}` on an already passive socket",
        "`m4 = timeout` — the oracle owes no second `{{tcp_passive,S}}`",
    );
}

// -----------------------------------------------------------------------------------------------
// Test 6 — the channel carries nothing for a passive socket
// -----------------------------------------------------------------------------------------------

/// A socket that was never armed — and one explicitly set `{active, false}` — puts **nothing** on
/// the active-input channel: `drain_active_inputs` stays empty however much the peer writes.
///
/// This is the channel's own boundary: a passive socket's bytes belong to the port's data queue
/// (the `{Port, {data, …}}` lane), never to `{tcp, S, Data}`. The oracle agrees — a passive
/// `gen_tcp` socket sends its owner nothing at all.
#[test]
fn a_passive_tcp_socket_delivers_nothing_on_the_active_channel() {
    let Some((mut session, mut peer)) =
        Session::connected_tcp("active input: passive socket")
    else {
        return;
    };

    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_PASSIVE)),
        vec![1],
        "the driver accepts `{{active,false}}`"
    );
    peer.write_all(b"hi")
        .expect("the loopback peer writes `hi`");
    session.assert_silent(
        "a write on a never-armed socket",
        "a passive `gen_tcp` socket sends its owner no message",
    );

    let reply = session.setopts(&record(INET_LOPT_ACTIVE, INET_PASSIVE));
    assert_eq!(reply, vec![1], "a second `{{active,false}}` is accepted");
    peer.write_all(b"again")
        .expect("the loopback peer writes `again`");
    session.assert_silent(
        "a write on an explicitly `{active,false}` socket",
        "a passive `gen_tcp` socket sends its owner no message",
    );
}

// -----------------------------------------------------------------------------------------------
// Test 7 — UDP: the source address, the client's own port, and an empty datagram
// -----------------------------------------------------------------------------------------------

/// A real datagram delivered to an `{active, once}` `udp_inet` port names the **client's** source
/// address and source port — decoded big-endian out of the seven-byte packed address — and an empty
/// datagram is a real delivery whose `Data` is `<<>>`.
///
/// Measured on the oracle (`gen_udp` on both ends, loopback):
/// `{udp,#Port<0.3>,{127,0,0,1},56678,<<104,105>>}` — where `56678` is the sender's own
/// `inet:sockname/1` port — and `{udp,#Port<0.3>,{127,0,0,1},56678,<<>>}` for a zero-length
/// datagram.
#[test]
fn a_real_datagram_names_the_clients_own_source_address_and_port() {
    let Some((mut session, bound)) = Session::bound_udp("active input: {active,once} over UDP")
    else {
        return;
    };
    let Some(client) = loopback_datagram_client("active input: the UDP client") else {
        return;
    };
    let Some(client_port) = client.local_addr().ok().map(|address| address.port()) else {
        skip_no_loopback("active input: the UDP client has no local address");
        return;
    };
    let target = ("127.0.0.1", bound);

    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    assert_eq!(
        client.send_to(b"hi", target).expect("the client sends `hi`"),
        2,
        "the whole datagram goes out"
    );
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(packets.len(), 1, "one datagram is one delivery, staged {packets:?}");
    let packet = &packets[0];
    let message = session.message(packet);
    assert_eq!(arity(message), 5, "{{udp, S, IP, Port, Data}} is a 5-tuple");
    session.assert_atom("the tag", element(message, 0), "udp");
    session.assert_port("a `{{udp, S, IP, Port, Data}}` message", message);
    assert_eq!(
        session.print(element(message, 2)),
        "{127,0,0,1}",
        "the loopback source address is the `IP` element"
    );
    assert_eq!(
        element(message, 3),
        make_small(i64::from(client_port)),
        "the `Port` element is the sender's own source port ({client_port}), decoded big-endian \
         from the packed address"
    );
    assert_eq!(
        binary(element(message, 4)),
        b"hi",
        "the datagram's bytes are the `Data` element"
    );
    assert_eq!(
        session.render(packet),
        format!("{{udp,{},{{127,0,0,1}},{client_port},<<104,105>>}}", session.port_text()),
        "the oracle prints `{{udp,#Port<0.3>,{{127,0,0,1}},<the client's port>,<<104,105>>}}`"
    );

    // An empty datagram is a real delivery whose payload is the empty binary.
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts the re-arm"
    );
    assert_eq!(
        client
            .send_to(&[], target)
            .expect("the client sends an empty datagram"),
        0,
        "a zero-length datagram is a real send"
    );
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(
        packets.len(),
        1,
        "an empty datagram is still one delivery, staged {packets:?}"
    );
    let message = session.message(&packets[0]);
    assert_eq!(
        binary(element(message, 4)),
        Vec::<u8>::new(),
        "the oracle's `Data` for an empty datagram is `<<>>`, not `[]` and not `undefined`"
    );
    assert_eq!(
        session.render(&packets[0]),
        format!("{{udp,{},{{127,0,0,1}},{client_port},<<>>}}", session.port_text()),
        "the oracle prints `{{udp,#Port<0.3>,{{127,0,0,1}},<the client's port>,<<>>}}`"
    );
}

// -----------------------------------------------------------------------------------------------
// Test 8 — UDP in `{mode, list}`
// -----------------------------------------------------------------------------------------------

/// The datagram family's `{mode, list}` rendering: the same datagram's payload as a charlist, with
/// the address and port elements unchanged.
///
/// Measured on the oracle: `{udp,#Port<0.3>,{127,0,0,1},56678,[104,105]}` (with `is_list(Data)`
/// true) — the tree passes through exactly as `{tcp, …}` does, because both go through the one
/// stream shaper.
#[test]
fn a_mode_list_udp_delivery_renders_the_payload_as_a_charlist() {
    let Some((mut session, bound)) =
        Session::bound_udp("active input: UDP {mode,list}")
    else {
        return;
    };
    let Some(client) = loopback_datagram_client("active input: the UDP list-mode client") else {
        return;
    };
    let Some(client_port) = client.local_addr().ok().map(|address| address.port()) else {
        skip_no_loopback("active input: the UDP client has no local address");
        return;
    };

    assert_eq!(
        session.setopts(&record(INET_LOPT_MODE, INET_MODE_LIST)),
        vec![1],
        "the driver accepts `{{mode,list}}`"
    );
    assert_eq!(
        session.setopts(&record(INET_LOPT_ACTIVE, INET_ONCE)),
        vec![1],
        "the driver accepts `{{active,once}}`"
    );
    assert_eq!(
        client
            .send_to(b"hi", ("127.0.0.1", bound))
            .expect("the client sends `hi`"),
        2,
        "the whole datagram goes out"
    );
    let packets = session.collect(DELIVERY_BOUND);
    assert_eq!(packets.len(), 1, "one datagram is one delivery, staged {packets:?}");
    let packet = &packets[0];
    assert_eq!(
        packet.data_mode,
        StreamType::Binary,
        "the packet's own mode is still the open-time `[binary]` stream (see the module docs)"
    );
    let message = session.message(packet);
    assert_eq!(
        list(element(message, 4)),
        vec![make_small(104), make_small(105)],
        "the `[list]` payload is one flat char list of the datagram's bytes"
    );
    assert_eq!(
        session.render(packet),
        format!("{{udp,{},{{127,0,0,1}},{client_port},[104,105]}}", session.port_text()),
        "the oracle prints `{{udp,#Port<0.3>,{{127,0,0,1}},<the client's port>,[104,105]}}`"
    );
}

/// A real loopback `std::net::UdpSocket` bound to a kernel-assigned port, or `None` with the loud
/// banner (the environment cannot host a datagram case).
fn loopback_datagram_client(label: &str) -> Option<UdpSocket> {
    match UdpSocket::bind("127.0.0.1:0") {
        Ok(client) => Some(client),
        Err(error) => {
            skip_no_loopback(&format!("{label}: UdpSocket::bind(127.0.0.1:0) failed: {error}"));
            None
        }
    }
}

// -----------------------------------------------------------------------------------------------
// Test 9 — the oracle
// -----------------------------------------------------------------------------------------------

/// The system `erl`'s own rendering of every shape the cases above assert, and of the sequences they
/// deliberately do **not** claim (the passive window).
///
/// Each probe is a real `gen_tcp`/`gen_udp` listener plus a real loopback `std::net`-equivalent
/// client in another Erlang process, armed through `inet:setopts/2` — i.e. the same wire the cases
/// above drive through `port_control`. The port term varies per run, so both sides are compared with
/// their `#Port<…>` masked; every other byte is compared exactly.
///
/// A missing toolchain — or a probe that cannot complete, which is what a sandbox without loopback
/// looks like — is a **loud skip**, never a silent pass.
#[test]
fn the_oracle_renders_every_shape_these_cases_assert() {
    let Some(output) = oracle_probe(TCP_PROBE) else {
        skip_oracle_check("the `{active,…}` message shapes (TCP)");
        return;
    };
    for (label, expected) in [
        ("m1=", "m1={tcp,#Port<0.5>,<<104,101,108,108,111>>}"),
        ("m2=", "m2=timeout"),
        // The re-arm's inline read takes what the socket buffered during the passive window
        // (`<<"world">>`), not the `<<"again">>` written after it — see the module docs' measured
        // differences. This runtime delivers the fresh write instead, which case 1 asserts.
        ("m3=", "m3={tcp,#Port<0.5>,<<119,111,114,108,100>>}"),
        // The probe's `{mode,list}` arm inherits the passive window's buffered bytes
        // (`<<"again">>`), so the oracle renders THAT payload as a char list here. Case 2 asserts
        // this runtime renders its own `<<"hello">>` write the same way (`[104,101,108,108,111]`).
        ("mode_list=", "mode_list={tcp,#Port<0.5>,[97,103,97,105,110]}"),
        ("mode_list_is_list=", "mode_list_is_list=true"),
    ] {
        assert_eq!(
            mask_ports(&oracle_line(&output, label)),
            mask_ports(expected),
            "the oracle's `{label}` line"
        );
    }

    let Some(output) = oracle_probe(FRAMING_PROBE) else {
        skip_oracle_check("the `{packet,2}`/`{header,2}` shapes");
        return;
    };
    for (label, expected) in [
        ("packet2=", "packet2={tcp,#Port<0.5>,<<104,101,108,108,111>>}"),
        (
            "header2=",
            "header2={tcp,#Port<0.5>,[104,101|<<108,108,111>>]}",
        ),
    ] {
        assert_eq!(
            mask_ports(&oracle_line(&output, label)),
            mask_ports(expected),
            "the oracle's `{label}` line"
        );
    }

    let Some(output) = oracle_probe(COUNTER_PROBE) else {
        skip_oracle_check("the `{active,N}` counter shapes");
        return;
    };
    for (label, expected) in [
        ("one=", "one={tcp,#Port<0.5>,<<111,110,101>>}"),
        ("two=", "two={tcp,#Port<0.5>,<<116,119,111>>}"),
        ("passive=", "passive={tcp_passive,#Port<0.5>}"),
        ("silent=", "silent=timeout"),
        ("after_false=", "after_false=timeout"),
    ] {
        assert_eq!(
            mask_ports(&oracle_line(&output, label)),
            mask_ports(expected),
            "the oracle's `{label}` line"
        );
    }

    let Some(output) = oracle_probe(UDP_PROBE) else {
        skip_oracle_check("the `{udp, S, IP, Port, Data}` shapes");
        return;
    };
    let client_port = oracle_field(&output, "client_port=");
    for (label, expected) in [
        (
            "udp_binary=",
            format!("udp_binary={{udp,#Port<0.3>,{{127,0,0,1}},{client_port},<<104,105>>}}"),
        ),
        (
            "udp_empty=",
            format!("udp_empty={{udp,#Port<0.3>,{{127,0,0,1}},{client_port},<<>>}}"),
        ),
        (
            "udp_list=",
            format!("udp_list={{udp,#Port<0.3>,{{127,0,0,1}},{client_port},[104,105]}}"),
        ),
    ] {
        assert_eq!(
            mask_ports(&oracle_line(&output, label)),
            mask_ports(&expected),
            "the oracle's `{label}` line (with the sender's own source port {client_port})"
        );
    }
    assert_eq!(
        oracle_line(&output, "udp_list_is_list="),
        "udp_list_is_list=true",
        "the oracle's `[list]` datagram payload really is a list"
    );
}

/// `{active,once}` binary, the spent grant, the re-arm — plus the same `[binary]` socket switched to
/// `{mode,list}`, all over real loopback TCP.
const TCP_PROBE: &str = "begin \
     {ok,L} = gen_tcp:listen(0,[binary,{ip,{127,0,0,1}},{active,false}]), \
     {ok,{_,P}} = inet:sockname(L), \
     Self = self(), \
     _ = spawn(fun() -> {ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false}]), \
     Self ! {client,C}, receive after 20000 -> ok end end), \
     {ok,A} = gen_tcp:accept(L), \
     C = receive {client,X} -> X after 5000 -> exit(no_client) end, \
     inet:setopts(A,[{active,once}]), \
     gen_tcp:send(C,<<\"hello\">>), \
     M1 = receive X1 -> X1 after 2000 -> timeout end, \
     gen_tcp:send(C,<<\"world\">>), \
     M2 = receive X2 -> X2 after 800 -> timeout end, \
     inet:setopts(A,[{active,once}]), \
     gen_tcp:send(C,<<\"again\">>), \
     M3 = receive X3 -> X3 after 2000 -> timeout end, \
     inet:setopts(A,[{mode,list},{active,once}]), \
     gen_tcp:send(C,<<\"hello\">>), \
     M4 = receive X4 -> X4 after 2000 -> timeout end, \
     io:format(\"m1=~w~nm2=~w~nm3=~w~nmode_list=~w~nmode_list_is_list=~w~n\", \
     [M1,M2,M3,M4,is_list(element(3,M4))]), \
     gen_tcp:close(A), gen_tcp:close(L) end";

/// `{packet,2}` and `{header,2}`, each on its own real loopback connection.
const FRAMING_PROBE: &str = "begin \
     {ok,L} = gen_tcp:listen(0,[binary,{ip,{127,0,0,1}},{active,false},{packet,2}]), \
     {ok,{_,P}} = inet:sockname(L), \
     Self = self(), \
     _ = spawn(fun() -> {ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,2}]), \
     Self ! {client,C}, receive after 20000 -> ok end end), \
     {ok,A} = gen_tcp:accept(L), \
     C = receive {client,X} -> X after 5000 -> exit(no_client) end, \
     inet:setopts(A,[{active,once}]), \
     gen_tcp:send(C,<<\"hello\">>), \
     Packet2 = receive X1 -> X1 after 2000 -> timeout end, \
     gen_tcp:close(A), gen_tcp:close(L), \
     {ok,L2} = gen_tcp:listen(0,[binary,{ip,{127,0,0,1}},{active,false},{header,2}]), \
     {ok,{_,P2}} = inet:sockname(L2), \
     _ = spawn(fun() -> {ok,C2} = gen_tcp:connect({127,0,0,1},P2,[binary,{active,false}]), \
     Self ! {client2,C2}, receive after 20000 -> ok end end), \
     {ok,A2} = gen_tcp:accept(L2), \
     C2 = receive {client2,X2} -> X2 after 5000 -> exit(no_client2) end, \
     inet:setopts(A2,[{active,once}]), \
     gen_tcp:send(C2,<<\"hello\">>), \
     Header2 = receive X3 -> X3 after 2000 -> timeout end, \
     io:format(\"packet2=~w~nheader2=~w~n\",[Packet2,Header2]), \
     gen_tcp:close(A2), gen_tcp:close(L2) end";

/// `{active,2}` over three writes, then a `{active,false}` that owes no second passive message.
const COUNTER_PROBE: &str = "begin \
     {ok,L} = gen_tcp:listen(0,[binary,{ip,{127,0,0,1}},{active,false}]), \
     {ok,{_,P}} = inet:sockname(L), \
     Self = self(), \
     _ = spawn(fun() -> {ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false}]), \
     Self ! {client,C}, receive after 20000 -> ok end end), \
     {ok,A} = gen_tcp:accept(L), \
     C = receive {client,X} -> X after 5000 -> exit(no_client) end, \
     inet:setopts(A,[{active,2}]), \
     gen_tcp:send(C,<<\"one\">>), \
     One = receive X1 -> X1 after 2000 -> timeout end, \
     gen_tcp:send(C,<<\"two\">>), \
     Two = receive X2 -> X2 after 2000 -> timeout end, \
     Passive = receive X3 -> X3 after 2000 -> timeout end, \
     gen_tcp:send(C,<<\"three\">>), \
     Silent = receive X4 -> X4 after 800 -> timeout end, \
     inet:setopts(A,[{active,false}]), \
     AfterFalse = receive X5 -> X5 after 800 -> timeout end, \
     io:format(\"one=~w~ntwo=~w~npassive=~w~nsilent=~w~nafter_false=~w~n\", \
     [One,Two,Passive,Silent,AfterFalse]), \
     gen_tcp:close(A), gen_tcp:close(L) end";

/// UDP: `{active,once}` binary, an empty datagram, and `{mode,list}` — with the sender's own
/// `inet:sockname/1` port printed so the expected literals can name it.
const UDP_PROBE: &str = "begin \
     {ok,A} = gen_udp:open(0,[binary,{ip,{127,0,0,1}},{active,false}]), \
     {ok,{_,P}} = inet:sockname(A), \
     {ok,C} = gen_udp:open(0,[binary,{ip,{127,0,0,1}},{active,false}]), \
     {ok,{_,CP}} = inet:sockname(C), \
     inet:setopts(A,[{active,once}]), \
     gen_udp:send(C,{127,0,0,1},P,<<\"hi\">>), \
     M1 = receive X1 -> X1 after 2000 -> timeout end, \
     inet:setopts(A,[{active,once}]), \
     gen_udp:send(C,{127,0,0,1},P,<<>>), \
     M2 = receive X2 -> X2 after 2000 -> timeout end, \
     inet:setopts(A,[{mode,list},{active,once}]), \
     gen_udp:send(C,{127,0,0,1},P,<<\"hi\">>), \
     M3 = receive X3 -> X3 after 2000 -> timeout end, \
     io:format(\"client_port=~w~nudp_binary=~w~nudp_empty=~w~nudp_list=~w~nudp_list_is_list=~w~n\", \
     [CP,M1,M2,M3,is_list(element(5,M3))]), \
     gen_udp:close(A), gen_udp:close(C) end";

/// Runs one oracle probe and answers its whole stdout, or `None` when `erl` is not usable (the
/// caller prints the loud banner).
fn oracle_probe(expr: &str) -> Option<String> {
    crate::port::tests::erl_eval(expr)
}

/// The oracle output's `label…` line, which must exist — a successful probe that omits a line is a
/// genuine mismatch, not an environment problem.
fn oracle_line(output: &str, label: &str) -> String {
    output
        .lines()
        .find(|line| line.starts_with(label))
        .unwrap_or_else(|| panic!("the oracle printed no {label:?} line in:\n{output}"))
        .to_string()
}

/// The value part of the oracle's `label<value>` line.
fn oracle_field(output: &str, label: &str) -> String {
    let line = oracle_line(output, label);
    line[label.len()..].to_string()
}

/// `text` with every `#Port<…>` masked, so an expected literal can name the oracle's port number and
/// still compare against a run in which the port number differs.
fn mask_ports(text: &str) -> String {
    const OPEN: &str = "#Port<";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let Some(close) = tail.find('>') else {
            out.push_str(tail);
            return out;
        };
        out.push_str("#Port<P>");
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}
