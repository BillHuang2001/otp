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

//! The **driver→owner active-mode input channel**: [`beam_io`]'s term-free
//! [`ActiveInput`]/[`ActiveInputEvent`] turned into the messages an `{active, …}` socket's owner
//! is owed.
//!
//! # What it is for
//!
//! A socket in `{active, true}` / `{active, once}` / `{active, N}` mode does **not** park its
//! input in the port's byte queue for the owner's next read: the C driver *sends a message to
//! the port's owner* from its read-readiness callback (`tcp_message`,
//! `erts/emulator/drivers/common/inet_drv.c:3979`, and `packet_binary_message`, `:4110`). This
//! module is that message family's runtime half.
//!
//! | event | port kind | message | C |
//! |---|---|---|---|
//! | [`ActiveInputEvent::Data`] | `tcp_inet` | `{tcp, S, Data}` | `tcp_message`, `:3979-4020`, and `tcp_binary_message`, `:4022-4055` |
//! | [`ActiveInputEvent::Data`] | `udp_inet` | `{udp, S, IP, Port, Data}` | `packet_binary_message`, `:4105-4160` |
//! | [`ActiveInputEvent::Passive`] | `tcp_inet` | `{tcp_passive, S}` | `packet_passive_message`, `:4215` |
//! | [`ActiveInputEvent::Passive`] | `udp_inet` | `{udp_passive, S}` | the same builder, the other branch |
//!
//! Without this half, `gen_tcp`/`gen_udp` in any active mode never delivers: `beam_io`'s driver
//! side (`drivers/inet/active.rs`, `drivers/inet/udp/active.rs`) already implements the whole
//! `{active, …}` state machine and stages one delivery per unit, and nothing consumed it.
//!
//! # The three steps
//!
//! 1. **Drain** ([`PortRegistry::drain_active_inputs`]) — pull everything
//!    [`beam_io::PortRegistry::take_active_inputs`] has staged and bind each delivery to the
//!    facts its rendering still needs (`owner`, the stream mode, the port's **kind**) *now*,
//!    while the port's metadata is still readable.
//! 2. **Report** ([`PortRegistry::drain_active_inputs`] returns them) — a drained
//!    [`ActiveInputPacket`] is this module's own term-free output, and the caller (the port
//!    provider, `crate::port`) wraps each packet in its own event and delivers it to
//!    `packet.owner`, exactly as it does for a
//!    [`CompletionPacket`](crate::port::completion::CompletionPacket). Nothing here touches an
//!    event enum or a `pending_events` queue: the packet already names its recipient, its stream
//!    mode and its socket kind, so the provider needs no second table read to deliver it.
//! 3. **Render** ([`RuntimeContext::active_input_message`]) — build the Erlang tuple, after the
//!    registry's borrow has been released, on the runtime's pointer-stable `-eval` arena and
//!    through the **same** stream shaper every other port message uses.
//!
//! # One delivered unit is a `beam_io` decision, not this module's
//!
//! `beam_io` documents what one delivery is: under a framing `{packet, N}` **one frame**, with
//! the framing header **already stripped**; under `{packet, raw}` **one read burst**. And the
//! read's `header` is **not** the framing header — for a stream port it is the `{header, N}`
//! option (the C's `desc->hsz`, `inet_drv.c:6931`), for a datagram port the 7-byte packed source
//! address. So this module renders `Data` from the read's own header/body split and never
//! re-frames anything.
//!
//! # The stream mode is `PortMeta::data_mode`, and it is a seam
//!
//! `{mode, list|binary}` never reaches `beam_io` (`drivers/inet/CONTEXT.md` §"Shape"): the
//! runtime decides list vs binary, and the C's rule is
//! `(desc->mode == INET_MODE_LIST) || (hsz > len)` ⇒ one flat char list
//! (`tcp_message`, `inet_drv.c:3985`) — which is exactly
//! [`RuntimeContext::make_stream_payload`](crate::port::RuntimeContext)'s `[list]` arm, so the
//! shaper is reused rather than reimplemented.
//!
//! There is exactly **one** call site for the mode
//! ([`PortMeta::data_mode`](crate::port::PortMeta)), so a `{mode, …}` `setopts` mirror can be
//! adopted there later without touching this module.
//!
//! # Where the drain happens
//!
//! An active delivery is staged by the same driver callbacks a completion is staged from
//! (`ready_input`, and a `setopts` control op that reads inline), so the drain follows **every**
//! point a driver callback can run — immediately beside each
//! [`PortRegistry::drain_io_completions`](crate::port::PortRegistry) call: [`write_command`]'s two
//! branches, [`port_control`], [`close`], [`pump`]'s round and [`pump`]'s "nothing live to poll"
//! early return. `beam_io` keeps a staged delivery on the port until it is drained and reports
//! deliveries harvested from ports closed since the last drain
//! ([`beam_io::PortRegistry::take_active_inputs`]), so nothing is stranded by a late drain and
//! nothing is reported twice.
//!
//! # A delivery for a port that has left the runtime is dropped
//!
//! The owner and the mode are resolved at **drain** time. A delivery whose port has no metadata
//! any more — the port was closed while its delivery was still staged — has no recoverable
//! recipient and is dropped, the rule the completion channel applies to a completion with no
//! recipient and the rule [`PortRegistry::close`](crate::port::PortRegistry) applies to an already
//! harvested event for the closing port. And because a packet carries the `owner` it resolved at
//! drain time, a dropped delivery can never be re-attributed to a later port that **reuses** that
//! number: the drop is final and the draining `Vec` is what carried it.
//!
//! [`write_command`]: crate::port::PortRegistry
//! [`port_control`]: crate::port::PortRegistry
//! [`close`]: crate::port::PortRegistry
//! [`pump`]: crate::port::PortRegistry

use beam_io::{ActiveInput, ActiveInputEvent, ReadMessage};
use beam_process::{PortId, ProcessId};
use beam_term::{make_atom, make_small, Eterm};

use crate::error;
use crate::port::completion::terms_tuple;
use crate::port::{driver_path_name, PortMeta, PortRegistry, RuntimeContext, StreamType};

/// The address tag of an explicit IPv4 address in the packed-address record (`INET_AF_INET`,
/// `erts/emulator/drivers/common/inet_drv.c:757`) — the only family
/// [`beam_io`]'s datagram driver packs (`wire::pack_address`, `:518`, which is `pub(super)` and
/// therefore decoded here rather than imported).
const INET_AF_INET: u8 = 1;

/// The **length** of the packed IPv4 address record: `[INET_AF_INET, port_hi, port_lo, a, b, c,
/// d]` (`wire::pack_address`, `beam_io/src/drivers/inet/wire.rs:518`).
///
/// The record is the delivered read's **header** for a datagram port, and it is decoded into the
/// `IP`/`Port` elements of `{udp, S, IP, Port, Data}` — the C's `load_address`
/// (`inet_drv.c:1916`) applied to the bytes `packet_binary_message` (`:4110`) finds in front of
/// the payload.
const PACKED_IPV4_ADDRESS_LEN: usize = 7;

/// What a driver port is, in the only terms the active-input messages need.
///
/// The C picks the message family from the socket's own protocol
/// (`packet_passive_message`'s `desc->proto`, `inet_drv.c:4215`; `tcp_message` vs
/// `packet_binary_message` at the delivery sites), and a driver cannot spell that: the channel
/// is deliberately term-free, so `beam_io` hands up one `ActiveInputEvent` vocabulary and the
/// **runtime** names the message. The kind is therefore resolved here, from the port's driver
/// name ([`PortMeta::command`](crate::port::PortMeta), whose first blank-separated word is the
/// driver name — the same rule the driver search applies, [`driver_path_name`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivePortKind {
    /// A `tcp_inet` port: `{tcp, S, Data}` / `{tcp_passive, S}`.
    Tcp,
    /// A `udp_inet` port: `{udp, S, IP, Port, Data}` / `{udp_passive, S}`.
    Udp,
    /// A port whose driver is neither socket driver.
    ///
    /// No in-tree driver but the two socket drivers stages an active delivery, so this kind is
    /// a driver this runtime cannot name a message for: a delivery on it is a **typed
    /// capability gap** ([`BeamError`](beam_common::BeamError) through
    /// [`crate::error::unsupported`]), never a fabricated `{tcp, …}` and never a silent drop.
    Other,
}

/// The driver name of a socket kind, or `None` for [`ActivePortKind::Other`].
///
/// The two names are `beam_io`'s own constants
/// ([`beam_io::drivers::inet::DRIVER_NAME`] = `"tcp_inet"`,
/// [`beam_io::drivers::inet::UDP_DRIVER_NAME`] = `"udp_inet"`). Both spellings are matched
/// literally rather than referenced so this module never has to hold a driver in hand to name a
/// port — the metadata's `command` is all it reads.
fn kind_of_driver(driver_name: &str) -> ActivePortKind {
    match driver_name {
        "tcp_inet" => ActivePortKind::Tcp,
        "udp_inet" => ActivePortKind::Udp,
        _ => ActivePortKind::Other,
    }
}

impl PortMeta {
    /// How this port renders a delivered payload: the `binary`/`list` stream option.
    ///
    /// **This is the single call site for the mode** in the active-input channel, and it is the
    /// seam a `{mode, …}` `setopts` mirror adopts later: `inet:setopts(S, [{mode, list|binary}])`
    /// never reaches [`beam_io`] (`drivers/inet/CONTEXT.md` §"Shape" — list vs binary is the
    /// runtime's decision), so a port whose mode changes after `open_port/2` has its new mode
    /// nowhere but an Erlang-side mirror. When that mirror lands, this method consults it and
    /// nothing else in this module changes.
    pub(crate) fn data_mode(&self) -> StreamType {
        self.stream
    }
}

/// One drained `{active, …}` delivery, bound to the port that produced it **and** to everything
/// its rendering still needs once that port's metadata may be gone.
///
/// Shaped exactly like [`CompletionPacket`](crate::port::completion::CompletionPacket): the
/// `owner`/`data_mode`/`kind` triple is resolved at **drain** time
/// ([`PortRegistry::drain_active_inputs`]) rather than at delivery time, so nothing is read from
/// a port table the delivering route may no longer hold. It carries no term: the payload travels
/// as the [`ReadMessage`] `beam_io` handed up, and every atom is interned only when the message
/// is built.
#[derive(Debug)]
pub(crate) struct ActiveInputPacket {
    /// The port whose driver produced the delivery (the C's `desc->dport`), i.e. the `S` element
    /// of every message family here.
    pub(crate) port: PortId,
    /// The process the message belongs to — the port's owner, the C's delivery target.
    pub(crate) owner: ProcessId,
    /// How the payload is rendered (list vs binary) — the port's stream mode at drain time, read
    /// through [`PortMeta::data_mode`] so there is exactly one call site for the mode.
    pub(crate) data_mode: StreamType,
    /// Which socket driver the port belongs to, and therefore which message family it names.
    pub(crate) kind: ActivePortKind,
    /// What the driver handed up, untouched: the payload with its header/body split, or the
    /// passive transition.
    pub(crate) event: ActiveInputEvent,
}

impl PortRegistry {
    /// Drains every `{active, …}` delivery `beam_io` has staged into term-free
    /// [`ActiveInputPacket`]s, binding each one's owner, stream mode and socket kind from the
    /// port's metadata **now**.
    ///
    /// A delivery whose port has no metadata (closed while its delivery was still staged) has no
    /// recoverable recipient and is dropped — see the module docs on why that drop is final and
    /// cannot be inherited by a port that later reuses the number.
    ///
    /// Call this immediately after every
    /// [`drain_io_completions`](PortRegistry::drain_io_completions): the two channels are staged
    /// by the same driver callbacks (`ready_input`, a `setopts` control op), so a point that owes
    /// one owes the other. The packets are term-free on purpose — the caller wraps each one in
    /// its own event and delivers it to [`ActiveInputPacket::owner`].
    pub(crate) fn drain_active_inputs(&mut self) -> Vec<ActiveInputPacket> {
        let drained = self.registry.take_active_inputs();
        let mut packets = Vec::with_capacity(drained.len());
        for ActiveInput { port, event } in drained {
            let Some(meta) = self.meta.get(&port.number()) else {
                continue;
            };
            packets.push(ActiveInputPacket {
                port,
                owner: meta.owner,
                data_mode: meta.data_mode(),
                kind: kind_of_driver(driver_path_name(&meta.command)),
                event,
            });
        }
        packets
    }
}

/// The `(IP, Port)` pair a datagram delivery's packed-address header names.
///
/// Decodes `[INET_AF_INET, port_hi, port_lo, a, b, c, d]` into the `{A,B,C,D}` tuple's bytes and
/// the source port — the C's `load_address` (`inet_drv.c:1916`) over `pack_address`'s record
/// (`wire.rs:518`). The 16-bit port field is **big-endian** (a socket bound to port 23456 =
/// `0x5BA0` arrives as `[1, 0x5B, 0xA0, …]`, verified against the oracle), so it is
/// `u16::from_be_bytes`.
///
/// # Errors
///
/// The typed capability gap for a header that is not exactly one packed IPv4 record — a
/// truncated delivery, a record of another family (`INET_AF_INET6`, `INET_AF_ANY`,
/// `INET_AF_LOOPBACK`, none of which a **received** datagram can carry) or a longer record. The
/// alternative would be a fabricated address, and an address is the one element of `{udp, …}`
/// that must never be invented.
fn decode_packed_address(header: &[u8]) -> beam_common::Result<([u8; 4], u16)> {
    match header {
        [INET_AF_INET, port_hi, port_lo, a, b, c, d] => {
            Ok(([*a, *b, *c, *d], u16::from_be_bytes([*port_hi, *port_lo])))
        }
        _ => Err(error::unsupported(&format!(
            "an {{active,…}} datagram delivery's packed source address ({} bytes, expected one \
             {PACKED_IPV4_ADDRESS_LEN}-byte INET_AF_INET record)",
            header.len()
        ))),
    }
}

impl RuntimeContext<'_> {
    /// The message one [`ActiveInputPacket`] delivers to its owner.
    ///
    /// Built on the runtime's pointer-stable `-eval` arena (like every other port message) after
    /// the port registry's borrow has been released, with every atom interned in the runtime's
    /// **one shared** table so the printer can name it and a `receive` clause can match it.
    ///
    /// # Errors
    ///
    /// The typed capability gap for a port kind this module cannot name a message for (see
    /// [`ActivePortKind::Other`]) and for a malformed datagram address record; the arena's
    /// allocation failure when a term does not fit.
    pub(crate) fn active_input_message(
        &mut self,
        packet: &ActiveInputPacket,
    ) -> beam_common::Result<Eterm> {
        match &packet.event {
            ActiveInputEvent::Data(read) => self.active_data_message(packet, read),
            ActiveInputEvent::Passive => self.active_passive_message(packet),
        }
    }

    /// `{tcp, S, Data}` / `{udp, S, IP, Port, Data}` — one delivered input in active mode.
    ///
    /// `Data` goes through
    /// [`make_stream_payload`](crate::port::RuntimeContext) with [`StreamShape::Data`](crate::port::StreamShape),
    /// so the C's two rendering rules come from that one shaper and cannot drift: a `[list]` mode
    /// (or a `{header, N}` wider than the payload — `beam_io` hands that case up as a
    /// whole-payload header, which the `list` arm flattens) is one flat char list, and a
    /// `[binary]` mode is the bare payload binary, with the header bytes consed in front of it
    /// when the read carries any (`[H1..Hh | <<Data>>]`, `tcp_message`'s `LOAD_STRING_CONS`,
    /// `inet_drv.c:3990-4005`).
    ///
    /// The datagram family's address is **not** part of `Data`: the C renders it as its own
    /// elements (`packet_binary_message`, `:4110-4160`), so it is decoded out of the read's
    /// header and the payload is rendered from the body alone.
    fn active_data_message(
        &mut self,
        packet: &ActiveInputPacket,
        read: &ReadMessage,
    ) -> beam_common::Result<Eterm> {
        let port = packet.port.to_eterm();
        match packet.kind {
            ActivePortKind::Tcp => {
                let payload =
                    self.make_stream_payload(read, packet.data_mode, crate::port::StreamShape::Data)?;
                let tag = self.socket_atom("tcp");
                terms_tuple(self, &[tag, port, payload])
            }
            ActivePortKind::Udp => {
                let (ip, source_port) = decode_packed_address(&read.header)?;
                let [a, b, c, d] = ip.map(|byte| make_small(i64::from(byte)));
                let address = terms_tuple(self, &[a, b, c, d])?;
                // The datagram's payload is the read's **body only** (the header is the source
                // address, rendered as its own elements above), so it is shaped through the one
                // stream shaper from a header-less read — never a second byte-list builder.
                let payload_read = ReadMessage {
                    header: Vec::new(),
                    body: read.body.clone(),
                };
                let payload = self.make_stream_payload(
                    &payload_read,
                    packet.data_mode,
                    crate::port::StreamShape::Data,
                )?;
                let tag = self.socket_atom("udp");
                terms_tuple(
                    self,
                    &[
                        tag,
                        port,
                        address,
                        make_small(i64::from(source_port)),
                        payload,
                    ],
                )
            }
            ActivePortKind::Other => Err(unsupported_kind(packet, "an {active,…} delivery")),
        }
    }

    /// `{tcp_passive, S}` / `{udp_passive, S}` — an `{active, N}` counter reached zero.
    ///
    /// The C's `packet_passive_message` (`inet_drv.c:4215`) picks the atom from the socket's
    /// protocol (`tcp_passive` for `IPPROTO_TCP`, `udp_passive` otherwise); the protocol is the
    /// port's driver kind here, which is why the runtime — not `beam_io` — names it.
    fn active_passive_message(&mut self, packet: &ActiveInputPacket) -> beam_common::Result<Eterm> {
        let name = match packet.kind {
            ActivePortKind::Tcp => "tcp_passive",
            ActivePortKind::Udp => "udp_passive",
            ActivePortKind::Other => return Err(unsupported_kind(packet, "an {active,…} passive")),
        };
        let tag = self.socket_atom(name);
        terms_tuple(self, &[tag, packet.port.to_eterm()])
    }

    /// A socket-message tag atom (`tcp`, `udp`, `tcp_passive`, `udp_passive`) interned in the
    /// runtime's **one shared** table.
    fn socket_atom(&mut self, name: &str) -> Eterm {
        make_atom(self.runtime.code_server.atoms_mut().intern(name))
    }
}

/// The typed capability gap for a delivery on a port whose driver names no message family.
///
/// A capability gap, never a fabricated term and never a silent drop: the reason names the port,
/// the delivery and the driver the metadata holds, so an operator can tell which driver staged
/// it.
fn unsupported_kind(packet: &ActiveInputPacket, delivery: &str) -> beam_common::BeamError {
    error::unsupported(&format!(
        "{delivery} on port {} whose driver is not a socket driver this runtime can name a \
         message for",
        packet.port.number()
    ))
}

/// The channel's matrix suite — the rendered message per event/kind/mode, the packed-address
/// decoder, and the drain-time owner/mode/kind binding with its drop rule (see
/// `src/port/active_input_tests.rs`). A separate file so the matrix does not grow this one.
#[cfg(test)]
#[path = "active_input_tests.rs"]
mod tests;

/// The channel's end-to-end suite — real loopback sockets on `tcp_inet`/`udp_inet` ports, pumped
/// and drained exactly as the runtime's delivery arms will (see
/// `src/port/active_input_e2e_tests.rs`).
#[cfg(test)]
#[path = "active_input_e2e_tests.rs"]
mod e2e;
