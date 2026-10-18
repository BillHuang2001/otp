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

//! The **`{active, …}` input channel** matrix: the exact terms
//! [`RuntimeContext::active_input_message`](crate::port::RuntimeContext) builds for a
//! [`beam_io`] active delivery, and the drain-time owner/mode/kind binding that keeps them
//! renderable across a port's close.
//!
//! Two halves, both unit-level (no child, no descriptor, no `erl` oracle — the socket round trip
//! itself is `src/port/inet_matrix_tests.rs`'s and the driver suites' job):
//!
//! # 1. The render matrix
//!
//! | event | kind | mode | message | C |
//! |---|---|---|---|---|
//! | [`ActiveInputEvent::Data`] | `Tcp` | `[binary]`, no header | `{tcp, S, <<Data>>}` | `tcp_binary_message`, `inet_drv.c:4022` |
//! | [`ActiveInputEvent::Data`] | `Tcp` | `[binary]`, `{header,N}` | `{tcp, S, [H1..Hh\|<<Data>>]}` | `tcp_message`, `:3979-4005` |
//! | [`ActiveInputEvent::Data`] | `Tcp` | `[list]` | `{tcp, S, [D1..Dn]}` (one flat list) | the same builder's list branch |
//! | [`ActiveInputEvent::Data`] | `Udp` | either | `{udp, S, {A,B,C,D}, Port, Data}` | `packet_binary_message`, `:4105-4160` |
//! | [`ActiveInputEvent::Passive`] | `Tcp` | — | `{tcp_passive, S}` | `packet_passive_message`, `:4215` |
//! | [`ActiveInputEvent::Passive`] | `Udp` | — | `{udp_passive, S}` | the same builder |
//!
//! The payload shapes are the **shaper's** (`make_stream_payload`), so this suite pins that the
//! channel does not re-derive them: a `[list]` delivery is one flat char list whatever the
//! header/body split, and a `[binary]` one is the bare payload binary or the improper
//! `[H…|<<Data>>]`. Text comparisons go through the registered `~w` printer, which renders a char
//! list as its element list (`[104,105]`), so the shapes below are the printer's spelling of the
//! oracle's own terms (the oracle prints a *printable* list as a string — `"hi"` — the same term).
//!
//! The datagram address is decoded out of the read's **header** into the `IP`/`Port` elements
//! (the C's `load_address`, `:1916`), with the 16-bit port field **big-endian** — verified
//! against the oracle: a datagram from a socket bound to 23456 (`0x5BA0`) arrives as
//! `[1, 0x5B, 0xA0, 127, 0, 0, 1]` and prints as `{udp, S, {127,0,0,1}, 23456, <<"hi">>}`.
//!
//! # 2. The drain binding and the drop rule
//!
//! [`PortRegistry::drain_active_inputs`](crate::port::PortRegistry) binds each packet's
//! recipient, stream mode and socket kind while the port's metadata is still readable, and
//! **drops** a delivery whose port has left the runtime (the completion channel's rule for a
//! completion with no recoverable recipient). The tests drive a real registered port whose
//! driver stages a delivery from `output` and one from its `stop`, so both halves run through
//! production code.

use std::cell::Cell;
use std::collections::VecDeque;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_io::{ActiveInputEvent, Driver, DriverContext, ReadMessage};
use beam_process::{PortId, ProcessId};
use beam_term::{make_small, Eterm};

use super::{decode_packed_address, kind_of_driver, ActiveInputPacket, ActivePortKind};
use crate::config::RuntimeConfig;
use crate::port::driver_path_name;
use crate::port::{PortMeta, PortName, PortRegistry, RuntimeContext, StreamType};
use crate::Runtime;

// -----------------------------------------------------------------------------------------------
// Helpers shared by both halves
// -----------------------------------------------------------------------------------------------

/// Element `index` (0-based) of a tuple a builder just produced.
fn element(term: Eterm, index: usize) -> Eterm {
    terms::tuple_element(term, index)
        .unwrap_or_else(|error| panic!("element {index} of a built tuple: {error}"))
}

/// The arity of a tuple a builder just produced.
fn arity(term: Eterm) -> usize {
    terms::tuple_arity(term).unwrap_or_else(|error| panic!("the arity of a built tuple: {error}"))
}

/// A read with a header and a body — the `{header, N}` (or datagram address) split.
fn split(header: &[u8], body: &[u8]) -> ReadMessage {
    ReadMessage {
        header: header.to_vec(),
        body: Some(body.to_vec()),
    }
}

/// The packed source-address record a datagram delivery carries as its read **header**:
/// `[INET_AF_INET, port_hi, port_lo, a, b, c, d]` (`beam_io`'s `wire::pack_address`).
fn packed_address(port: u16, address: [u8; 4]) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![1, hi, lo, address[0], address[1], address[2], address[3]]
}

// -----------------------------------------------------------------------------------------------
// Half 1: the render matrix
// -----------------------------------------------------------------------------------------------

/// One runtime, one port, one owner — the arena every builder test builds its terms on.
///
/// The runtime outlives every message: a term is only meaningful while the heap it was built on
/// is alive, so the fixture is created once per test and borrowed for each message (the same
/// shape `src/port/completion_tests.rs` uses).
struct Fixture {
    /// The runtime whose `-eval` arena the messages are built on and whose printer renders them.
    rt: Runtime,
    /// The port every message names as its `S` element.
    port: PortId,
    /// The process a packet names as its recipient.
    owner: ProcessId,
}

impl Fixture {
    /// A fixture over a fresh default runtime.
    fn new() -> Fixture {
        Fixture {
            rt: Runtime::new(RuntimeConfig::default()).expect("the default runtime"),
            port: PortId::new(7),
            owner: ProcessId::new(1, 0),
        }
    }

    /// A packet for this fixture's port carrying `event`.
    fn packet(
        &self,
        kind: ActivePortKind,
        data_mode: StreamType,
        event: ActiveInputEvent,
    ) -> ActiveInputPacket {
        ActiveInputPacket {
            port: self.port,
            owner: self.owner,
            data_mode,
            kind,
            event,
        }
    }

    /// A `Data` packet for `read`.
    fn data(
        &self,
        kind: ActivePortKind,
        data_mode: StreamType,
        read: ReadMessage,
    ) -> ActiveInputPacket {
        self.packet(kind, data_mode, ActiveInputEvent::Data(read))
    }

    /// The message [`RuntimeContext::active_input_message`] builds for `packet`.
    fn message(&mut self, packet: &ActiveInputPacket) -> Eterm {
        let mut ctx = RuntimeContext::new(&mut self.rt);
        ctx.active_input_message(packet)
            .unwrap_or_else(|error| panic!("an active-input message for {packet:?}: {error}"))
    }

    /// The message [`RuntimeContext::active_input_message`] builds for `packet`, expecting the
    /// typed capability gap and returning its rendered reason.
    fn message_error(&mut self, packet: &ActiveInputPacket) -> BeamError {
        let mut ctx = RuntimeContext::new(&mut self.rt);
        match ctx.active_input_message(packet) {
            Ok(term) => panic!("{packet:?} must be a capability gap, not {term:?}"),
            Err(error) => error,
        }
    }

    /// `term` rendered by the registered `~w` printer.
    fn render(&mut self, term: Eterm) -> String {
        self.rt
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a built term: {error}"))
            .trim_end_matches('\n')
            .to_string()
    }

    /// The **name** of an atom term, so an assertion reads as the atom the C sends.
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

    /// Assert every message's `S` element is this fixture's port term.
    fn assert_sender(&self, message: Eterm) {
        assert_eq!(
            element(message, 1),
            self.port.to_eterm(),
            "the sender is the port's own term, never a fabricated atom"
        );
    }

    /// The `Data` of a `{tcp, S, Data}` message built for `packet`.
    fn tcp_data(&mut self, packet: &ActiveInputPacket) -> Eterm {
        let message = self.message(packet);
        assert_eq!(arity(message), 3, "{{tcp, S, Data}} is a 3-tuple");
        self.assert_atom("the tag", element(message, 0), "tcp");
        self.assert_sender(message);
        element(message, 2)
    }
}

/// A `[binary]` `tcp_inet` delivery with no `{header, N}` is the **bare payload binary** — the C's
/// `tcp_binary_message` (`inet_drv.c:4022-4055`) — and the oracle's `{tcp, S, <<"hi">>}`.
#[test]
fn a_tcp_delivery_in_binary_mode_is_the_bare_payload_binary() {
    let mut fixture = Fixture::new();
    let packet = fixture.data(
        ActivePortKind::Tcp,
        StreamType::Binary,
        ReadMessage::body_only(b"hi".to_vec()),
    );
    let data = fixture.tcp_data(&packet);
    assert_eq!(
        terms::binary_bytes(data).expect("a `[binary]` TCP delivery is one binary"),
        b"hi",
        "the payload is the bare binary, not a list"
    );
    assert_eq!(fixture.render(data), "<<104,105>>");
}

/// The same delivery under the default (`[list]`) stream is **one flat char list** — the C's
/// `(desc->mode == INET_MODE_LIST)` branch (`inet_drv.c:3985`), the oracle's
/// `{tcp, S, "hi"}`.
#[test]
fn a_tcp_delivery_in_list_mode_is_one_flat_char_list() {
    let mut fixture = Fixture::new();
    let packet = fixture.data(
        ActivePortKind::Tcp,
        StreamType::List,
        ReadMessage::body_only(b"hi".to_vec()),
    );
    let data = fixture.tcp_data(&packet);
    assert_eq!(
        terms::list_elements(data).expect("a `[list]` TCP delivery is a proper list"),
        vec![make_small(104), make_small(105)],
        "one element per byte of the payload"
    );
    assert_eq!(fixture.render(data), "[104,105]");
}

/// A `{header, N}` delivery conses the header bytes in front of the payload binary — the C's
/// `LOAD_STRING_CONS` (`inet_drv.c:3990-4005`), the oracle's `{tcp, S, [104,101|<<"llo">>]}`
/// for `{header,2}` over `<<"hello">>`.
#[test]
fn a_tcp_delivery_carrying_a_header_cons_prefix_is_an_improper_list() {
    let mut fixture = Fixture::new();
    let packet = fixture.data(
        ActivePortKind::Tcp,
        StreamType::Binary,
        split(b"he", b"llo"),
    );
    let data = fixture.tcp_data(&packet);
    assert_eq!(
        fixture.render(data),
        "[104,101|<<108,108,111>>]",
        "the header bytes are list elements and the payload the binary tail"
    );
    // Under `[list]` the split is invisible: one flat char list of the whole payload.
    let packet = fixture.data(ActivePortKind::Tcp, StreamType::List, split(b"he", b"llo"));
    let data = fixture.tcp_data(&packet);
    assert_eq!(fixture.render(data), "[104,101,108,108,111]");
}

/// A `{header, N}` exactly as wide as the payload keeps a **present, empty** binary tail — the
/// oracle's `{tcp, S, [104,105|<<>>]}`, because the C's `LOAD_BUF2BINARY(buf + hsz, 0)` still
/// builds `<<>>`.
///
/// # The `hsz > len` residual
///
/// The C's list branch is `(mode == LIST) || (hsz > len)`, so a header **wider** than the payload
/// is one flat string with **no** binary tail (the oracle's `{tcp, S, "h"}`). `beam_io` hands that
/// case up as a whole-payload header with a present-but-empty body
/// (`drivers/inet/active.rs`'s `split_payload` clamps `hsz` to the payload and records the
/// divergence), which is byte-identical to the `hsz == len` case — so a `[binary]` consumer sees
/// `[104|<<>>]` where the oracle prints `"h"`. The two are indistinguishable without `beam_io`
/// carrying the raw `hsz`, and **nothing is lost or mis-ordered**: the same bytes in the same
/// order, and a `[list]` consumer — the case the C's branch exists for — is exact.
#[test]
fn a_header_as_wide_as_the_payload_keeps_an_empty_binary_tail() {
    let mut fixture = Fixture::new();
    let packet = fixture.data(ActivePortKind::Tcp, StreamType::Binary, split(b"hi", b""));
    let data = fixture.tcp_data(&packet);
    assert_eq!(
        fixture.render(data),
        "[104,105|<<>>]",
        "a present, empty body is the binary tail `<<>>`, never a proper list"
    );
    // The `hsz > len` shape `beam_io` can express: the whole payload as the header. In `[list]`
    // mode this is the oracle's flat string, which is what the branch exists for.
    let packet = fixture.data(ActivePortKind::Tcp, StreamType::List, split(b"h", b""));
    let data = fixture.tcp_data(&packet);
    assert_eq!(
        fixture.render(data),
        "[104]",
        "the oracle's \"h\", one flat list"
    );
    let packet = fixture.data(ActivePortKind::Tcp, StreamType::Binary, split(b"h", b""));
    let data = fixture.tcp_data(&packet);
    assert_eq!(
        fixture.render(data),
        "[104|<<>>]",
        "the recorded residual: the oracle prints \"h\" here (see the test doc)"
    );
}

/// A `udp_inet` delivery names the packed source address as its own `IP`/`Port` elements — the
/// C's `packet_binary_message` (`inet_drv.c:4105-4160`) over `load_address` (`:1916`) — and the
/// payload follows the same list/binary rule as a stream delivery.
///
/// The 16-bit port field is **big-endian**: the oracle reports a datagram from a socket bound to
/// 23456 (`0x5BA0`) as `{udp, S, {127,0,0,1}, 23456, <<"hi">>}`, measured.
#[test]
fn a_udp_delivery_names_the_source_address_as_a_tuple_and_a_big_endian_port() {
    let mut fixture = Fixture::new();
    let read = split(&packed_address(23456, [127, 0, 0, 1]), b"hi");
    let packet = fixture.data(ActivePortKind::Udp, StreamType::Binary, read);
    let message = fixture.message(&packet);
    assert_eq!(arity(message), 5, "{{udp, S, IP, Port, Data}} is a 5-tuple");
    fixture.assert_atom("the tag", element(message, 0), "udp");
    fixture.assert_sender(message);
    assert_eq!(
        fixture.render(element(message, 2)),
        "{127,0,0,1}",
        "the address is a 4-element tuple, not a list or a binary"
    );
    assert_eq!(
        element(message, 3),
        make_small(23456),
        "the source port is the small integer 23456, decoded big-endian (`0x5BA0`)"
    );
    assert_eq!(
        terms::binary_bytes(element(message, 4)).expect("a `[binary]` datagram payload"),
        b"hi",
        "the address bytes are not part of the payload"
    );
    // The default stream renders the payload as a char list; the address is unchanged.
    let packet = fixture.data(
        ActivePortKind::Udp,
        StreamType::List,
        split(&packed_address(23456, [127, 0, 0, 1]), b"hi"),
    );
    let message = fixture.message(&packet);
    assert_eq!(fixture.render(element(message, 4)), "[104,105]");
    assert_eq!(fixture.render(element(message, 2)), "{127,0,0,1}");
}

/// An **empty** datagram is still a delivery — the oracle's `{udp, S, {127,0,0,1}, 50339, <<>>}`
/// — so the body being present-but-empty is a `<<>>` payload, never a dropped message.
#[test]
fn an_empty_datagram_is_still_a_delivery() {
    let mut fixture = Fixture::new();
    let packet = fixture.data(
        ActivePortKind::Udp,
        StreamType::Binary,
        split(&packed_address(50339, [127, 0, 0, 1]), b""),
    );
    let message = fixture.message(&packet);
    assert_eq!(
        fixture.render(element(message, 4)),
        "<<>>",
        "an empty datagram's payload is the empty binary"
    );
}

/// A datagram whose packed address record is missing, truncated or of another family is the
/// **typed capability gap** — never a fabricated address, since the address is the one element of
/// `{udp, …}` that cannot be guessed.
#[test]
fn a_malformed_address_record_is_a_typed_capability_gap() {
    let mut fixture = Fixture::new();
    for (label, header) in [
        ("truncated", vec![1u8, 0x5b]),
        ("empty", Vec::new()),
        ("another family", vec![2u8; 19]),
        ("too long", vec![1u8, 0, 1, 127, 0, 0, 1, 0]),
        ("wrong tag", vec![9u8, 0, 1, 127, 0, 0, 1]),
    ] {
        let packet = fixture.data(
            ActivePortKind::Udp,
            StreamType::Binary,
            split(&header, b"hi"),
        );
        let error = fixture.message_error(&packet);
        let reason = crate::error::unsupported_reason(&error)
            .unwrap_or_else(|| panic!("{label}: the gap must be an `unsupported` raise"));
        assert!(
            reason.contains("packed source address"),
            "{label}: the reason must name what could not be decoded, got `{reason}`"
        );
    }
}

/// The passive transition is named from the port's **kind**: `{tcp_passive, S}` for a
/// `tcp_inet` port, `{udp_passive, S}` for a `udp_inet` one — the C's
/// `packet_passive_message` (`inet_drv.c:4215`) choosing on the socket protocol.
#[test]
fn the_passive_message_is_named_from_the_ports_kind() {
    let mut fixture = Fixture::new();
    for (kind, expected) in [
        (ActivePortKind::Tcp, "tcp_passive"),
        (ActivePortKind::Udp, "udp_passive"),
    ] {
        let packet = fixture.packet(kind, StreamType::Binary, ActiveInputEvent::Passive);
        let message = fixture.message(&packet);
        assert_eq!(arity(message), 2, "{{_passive, S}} is a 2-tuple");
        fixture.assert_atom("the passive tag", element(message, 0), expected);
        fixture.assert_sender(message);
    }
}

/// A delivery on a port whose driver is **not** a socket driver is a typed capability gap for
/// both events — neither a fabricated `{tcp, …}` nor a silent drop.
#[test]
fn a_delivery_on_a_non_socket_driver_is_a_typed_capability_gap() {
    let mut fixture = Fixture::new();
    for event in [
        ActiveInputEvent::Data(ReadMessage::body_only(b"hi".to_vec())),
        ActiveInputEvent::Passive,
    ] {
        let packet = fixture.packet(ActivePortKind::Other, StreamType::Binary, event);
        let error = fixture.message_error(&packet);
        let reason = crate::error::unsupported_reason(&error)
            .unwrap_or_else(|| panic!("a driverless delivery must be an `unsupported` raise"));
        assert!(
            reason.contains("not a socket driver"),
            "the reason must name the cause, got `{reason}`"
        );
    }
}

/// The two socket driver names map to their kinds and **everything else** — including a name
/// carrying extra words, which the driver search truncates at the first blank — is `Other`.
#[test]
fn the_port_kind_is_derived_from_the_driver_name() {
    assert_eq!(kind_of_driver("tcp_inet"), ActivePortKind::Tcp);
    assert_eq!(kind_of_driver("udp_inet"), ActivePortKind::Udp);
    for name in ["ram_file_drv", "efile", "fd", "", "tcp_inet_v2", "inet"] {
        assert_eq!(
            kind_of_driver(name),
            ActivePortKind::Other,
            "`{name}` names no socket driver, so no message family"
        );
    }
    // The metadata's `command` may carry arguments; the driver search truncates at the first
    // blank, and the drain applies the same rule.
    assert_eq!(driver_path_name("tcp_inet extra"), "tcp_inet");
}

/// The packed-address decoder is the C's `load_address` (`inet_drv.c:1916`) over `pack_address`'s
/// record: the 4-tuple's bytes as-is and the 16-bit port field big-endian.
#[test]
fn the_packed_address_decodes_to_a_big_endian_port_and_the_four_bytes() {
    assert_eq!(
        decode_packed_address(&[1, 0x5b, 0xa0, 127, 0, 0, 1]).expect("a packed IPv4 record"),
        ([127, 0, 0, 1], 23456)
    );
    assert_eq!(
        decode_packed_address(&[1, 0, 80, 10, 1, 2, 3]).expect("a packed IPv4 record"),
        ([10, 1, 2, 3], 80),
        "port 80 is `[0, 80]`, not `[80, 0]`"
    );
    assert_eq!(
        decode_packed_address(&[1, 0xff, 0xff, 0, 0, 0, 0]).expect("a packed IPv4 record"),
        ([0, 0, 0, 0], u16::MAX)
    );
}

// -----------------------------------------------------------------------------------------------
// Half 2: the drain binding and the drop rule
// -----------------------------------------------------------------------------------------------

/// The pid every registry-level fixture port belongs to.
const OWNER: ProcessId = ProcessId::new(11, 0);

/// A driver that stages one `{active, …}` delivery from `output` (the read-readiness path a
/// socket driver takes) and/or one from its teardown.
///
/// It registers no descriptor, holds no child and arms no timer, so every callback below is
/// reached by the runtime's own call sites — there is nothing to poll and nothing to wait for.
struct ActiveDriver {
    /// What the `output` callback hands up, once.
    read: Option<ActiveInputEvent>,
    /// What the teardown hands up, once.
    teardown: Option<ActiveInputEvent>,
}

impl ActiveDriver {
    /// A driver that stages `event` from `output`.
    fn delivering(event: ActiveInputEvent) -> ActiveDriver {
        ActiveDriver {
            read: Some(event),
            teardown: None,
        }
    }

    /// A driver that stages `event` while it is torn down.
    fn on_teardown(event: ActiveInputEvent) -> ActiveDriver {
        ActiveDriver {
            read: None,
            teardown: Some(event),
        }
    }
}

impl Driver for ActiveDriver {
    fn name(&self) -> &str {
        "tcp_inet"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let written = ctx.output().len();
        ctx.output().consume(written);
        match self.read.take() {
            Some(event) => {
                ctx.deliver_active_input(event)?;
                Ok(written)
            }
            None => Ok(written),
        }
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        match self.teardown.take() {
            Some(event) => ctx.deliver_active_input(event),
            None => Ok(()),
        }
    }
}

/// The `port_info` metadata of a `{spawn_driver, _}` port owned by `owner` on `stream`, whose
/// driver name is `command` — the block `open_port/2`'s driver arm builds, without a child, a
/// descriptor or a `Name` decode.
fn port_meta(owner: ProcessId, stream: StreamType, command: &str) -> PortMeta {
    PortMeta {
        command: command.to_string(),
        os_pid: None,
        stream,
        line: None,
        packet: None,
        notify_exit: false,
        monitors: Vec::new(),
        owner,
        // The `Name` form whose bytes come from the driver's own callbacks
        // (`PortName::bytes_from_driver_output`), which is the arm a socket driver takes.
        kind: PortName::SpawnDriver,
        driver_has_control: false,
        driver_terms: None,
        driver_atoms: None,
        driver_control_flags: None,
        driver_busy: None,
        driver_monitors: None,
        input: Cell::new(0),
        output: Cell::new(0),
        exit_status_taken: Cell::new(false),
        held_lines: VecDeque::new(),
        held_since: Cell::new(None),
    }
}

/// A registry holding `driver` as a **live** port owned by `owner`, named `command` and on
/// `stream`.
///
/// The id is a real one — [`PortRegistry::register`] allocates it from `beam_io`'s registry, so
/// `contains`, `close` and the metadata all agree.
fn registry_with(
    driver: ActiveDriver,
    owner: ProcessId,
    stream: StreamType,
    command: &str,
) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new().expect("a port registry");
    let id = registry
        .register(Box::new(driver), port_meta(owner, stream, command))
        .expect("the fixture port registers");
    (registry, id)
}

/// Every `{active, …}` delivery the provider has **staged but not yet reported**: the
/// [`PortEvent::ActiveInput`](crate::port::PortEvent) entries in the registry's pending lane,
/// plus anything a direct drain still finds.
///
/// The staging sites ([`PortRegistry::write_command`], [`PortRegistry::port_control`],
/// [`PortRegistry::close`], [`PortRegistry::pump`]) drain the channel beside the completions and
/// hold each packet as pending work, so this — not a bare
/// [`drain_active_inputs`](PortRegistry::drain_active_inputs) — is how a delivery is observed
/// from outside the provider. The trailing direct drain keeps the helper honest for a fixture
/// that has not gone through a staging site yet; on a registry that has, it answers nothing.
fn take_active_inputs(registry: &mut PortRegistry) -> Vec<ActiveInputPacket> {
    let mut out = Vec::new();
    let mut retained = Vec::new();
    for event in std::mem::take(&mut registry.pending_events) {
        match event {
            crate::port::PortEvent::ActiveInput { packet } => out.push(packet),
            other => retained.push(other),
        }
    }
    registry.pending_events = retained;
    out.extend(registry.drain_active_inputs());
    out
}

/// How many `{active, …}` deliveries the pending lane currently holds — a **non-consuming**
/// witness, so a test can assert a staging site produced one *and* keep it for the assertions
/// that follow.
fn staged_active_inputs(registry: &PortRegistry) -> usize {
    registry
        .pending_events
        .iter()
        .filter(|event| matches!(event, crate::port::PortEvent::ActiveInput { .. }))
        .count()
}

/// A live port's `{active, …}` delivery is bound to that port's **owner**, **stream mode** and
/// **socket kind** at drain time.
///
/// This is the ordinary path: a socket driver's read-readiness callback stages the delivery, and
/// the drain's own reader (right beside the completion channel's) resolves the recipient while the
/// metadata is still readable. [`PortRegistry::write_command`] is the staging site that drives it
/// here, so the packet is observed through the provider's own pending lane.
#[test]
fn a_live_ports_delivery_is_bound_to_its_owner_mode_and_kind_at_drain_time() {
    let read = ReadMessage::body_only(b"hi".to_vec());
    let (mut registry, id) = registry_with(
        ActiveDriver::delivering(ActiveInputEvent::Data(read.clone())),
        OWNER,
        StreamType::Binary,
        "tcp_inet",
    );
    registry
        .write_command(id, b"")
        .expect("the command reaches the driver");
    assert_eq!(
        staged_active_inputs(&registry),
        1,
        "the command's driver callback stages one delivery and the provider holds it as pending work"
    );
    let packets = take_active_inputs(&mut registry);
    assert_eq!(packets.len(), 1, "exactly one delivery, reported once");
    assert_eq!(packets[0].port, id, "the port the driver belongs to");
    assert_eq!(packets[0].owner, OWNER, "the drain binds the port's owner");
    assert_eq!(
        packets[0].data_mode,
        StreamType::Binary,
        "and the port's stream mode, which shapes the payload"
    );
    assert_eq!(
        packets[0].kind,
        ActivePortKind::Tcp,
        "and the socket kind, which names the message"
    );
    assert_eq!(
        packets[0].event,
        ActiveInputEvent::Data(read),
        "the driver's own delivery, untouched"
    );
    assert!(
        take_active_inputs(&mut registry).is_empty(),
        "the delivery is consumed by the drain, never reported twice"
    );
}

/// A `udp_inet` port's delivery is bound to the `Udp` kind, so the drain is what decides the
/// `{udp, …}` family — the driver never spells it.
#[test]
fn a_udp_ports_delivery_is_bound_to_the_udp_kind() {
    let read = split(&packed_address(80, [10, 1, 2, 3]), b"x");
    let (mut registry, id) = registry_with(
        ActiveDriver::delivering(ActiveInputEvent::Data(read)),
        OWNER,
        StreamType::List,
        "udp_inet",
    );
    registry.write_command(id, b"").expect("the command");
    let packets = take_active_inputs(&mut registry);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].kind, ActivePortKind::Udp);
    assert_eq!(packets[0].data_mode, StreamType::List);
}

/// A delivery staged **around a close** has no recoverable recipient once the port's metadata is
/// gone, so it is dropped — and the drop is **final**: the draining `Vec` carried it, so a later
/// drain cannot re-report it, and a port that reuses the number can never inherit it.
#[test]
fn a_delivery_for_a_port_that_has_left_the_runtime_is_dropped_once_and_never_re_reported() {
    let (mut registry, id) = registry_with(
        ActiveDriver::on_teardown(ActiveInputEvent::Data(ReadMessage::body_only(
            b"late".to_vec(),
        ))),
        OWNER,
        StreamType::Binary,
        "tcp_inet",
    );
    registry.close(id).expect("the port closes");
    assert!(
        !registry.contains(id),
        "the close removed the metadata a drain would otherwise read"
    );
    assert!(
        take_active_inputs(&mut registry).is_empty(),
        "a delivery whose port has left the runtime has no recipient and is dropped"
    );
    assert!(
        take_active_inputs(&mut registry).is_empty(),
        "and the drop is final: the delivery was consumed, never re-reported"
    );
}

/// A delivery the drain has already reported needs **nothing** from the port table: the packet
/// names its recipient, its stream mode and its socket kind itself, so a close that happens
/// afterwards cannot strand it or take it back.
///
/// This is what makes a drained delivery *owed work* rather than a cached port read — the caller
/// holds a message its owner is still due until it delivers it, whatever happens to the port in
/// the meantime.
#[test]
fn a_delivery_drained_before_a_close_is_still_deliverable_after_it() {
    let mut fixture = Fixture::new();
    let read = ReadMessage::body_only(b"hi".to_vec());
    let (mut registry, id) = registry_with(
        ActiveDriver::delivering(ActiveInputEvent::Data(read)),
        fixture.owner,
        StreamType::Binary,
        "tcp_inet",
    );
    registry
        .write_command(id, b"")
        .expect("the command reaches the driver");
    let packets = take_active_inputs(&mut registry);
    assert_eq!(packets.len(), 1, "the drain reports the staged delivery");
    registry.close(id).expect("the port closes");
    assert!(
        take_active_inputs(&mut registry).is_empty(),
        "the close staged nothing further: the packet above is the only one"
    );
    // The reported packet still renders to the oracle's message, naming the port that has just
    // gone — every fact the rendering needs was resolved by the drain.
    let message = fixture.message(&packets[0]);
    assert_eq!(arity(message), 3, "{{tcp, S, Data}} is a 3-tuple");
    fixture.assert_atom("the tag", element(message, 0), "tcp");
    assert_eq!(
        element(message, 1),
        id.to_eterm(),
        "the closed port is still the message's `S`, resolved at drain time"
    );
    assert_eq!(
        terms::binary_bytes(element(message, 2)).expect("a `[binary]` delivery is one binary"),
        b"hi",
        "and the payload the driver handed up"
    );
}

/// A packet names its port from **its own** `PortId`, never by looking one up, so no close can
/// re-attribute or purge a delivery: rendering a packet for a port this runtime has never held
/// still names that exact port.
#[test]
fn an_active_packet_names_its_port_from_the_packet_not_a_port_table() {
    let mut fixture = Fixture::new();
    let port = PortId::new(9);
    let packet = ActiveInputPacket {
        port,
        owner: OWNER,
        data_mode: StreamType::Binary,
        kind: ActivePortKind::Udp,
        event: ActiveInputEvent::Passive,
    };
    let message = fixture.message(&packet);
    assert_eq!(arity(message), 2, "{{udp_passive, S}} is a 2-tuple");
    fixture.assert_atom("the passive tag", element(message, 0), "udp_passive");
    assert_eq!(
        element(message, 1),
        port.to_eterm(),
        "the `S` element is the packet's own port id, never one re-derived from a table"
    );
}
