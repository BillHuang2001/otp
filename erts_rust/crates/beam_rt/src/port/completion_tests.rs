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

//! The driver→owner **completion** matrix: the exact terms
//! [`RuntimeContext::completion_message`](crate::port::RuntimeContext) builds for a
//! [`beam_io`] completion, and the drain-time owner/stream snapshot that keeps them
//! deliverable across a port's close.
//!
//! Two halves, both unit-level (no child, no descriptor, no `erl` oracle — that is
//! `src/port/driver_output_shape_tests.rs`'s and `src/port/inet_matrix_tests.rs`'s job):
//!
//! # 1. The builder/render matrix
//!
//! | completion | the C | message |
//! |---|---|---|
//! | [`InetReply::Ok`] | `inet_reply_ok`, `inet_drv.c:2689` | `{inet_reply, Port, ok}` |
//! | [`InetReply::Error`] | `inet_reply_error_am`, `:2719` | `{inet_reply, Port, {error, Atom}}` |
//! | `InetReply` + a caller tag | `inet_reply_finish`, `:2614-2637` | `{inet_reply, Port, Reply, Mref}` |
//! | [`InetAsyncResult::Ok`] | `send_async_ok`, `:2493` | `{inet_async, Port, Id, ok}` |
//! | [`InetAsyncResult::Data`] | `inet_async_data`, `:3162` | `{inet_async, Port, Id, {ok, Data}}` |
//! | [`InetAsyncResult::Port`] | `send_async_ok_port`, `:2513` | `{inet_async, Port, Id, {ok, Port2}}` |
//! | [`InetAsyncResult::Error`] | `send_async_error`, `:2539` | `{inet_async, Port, Id, {error, Atom}}` |
//!
//! The `Data` payload follows the **read's** header/body split, not a byte count: a UDP
//! datagram carries its packed source address as the read's header
//! ([`pack_address`](beam_io) — `[AF_INET, PortHi, PortLo, A, B, C, D]`), so a `[binary]`
//! socket renders `[1,PortHi,PortLo,A,B,C,D | <<Payload>>]` (improper) and a `[list]` socket
//! one flat char list with the address first; a TCP read (`hsz == 0`) is the bare payload
//! binary or its char list. Every header byte is delivered **verbatim**, as the C's
//! `LOAD_STRING` does — the 16-bit port field is the consumer's (`prim_inet`'s) to re-parse,
//! never the emulator's.
//!
//! The caller tag is the `term_to_binary(Mref, [local])` image `prim_inet:send/3` prefixes
//! its command with, carried **verbatim** through `beam_io`. The suite builds that image with
//! this workspace's own encoder (a real reference, not hand-rolled bytes) and pins the fourth
//! element's **identity** — the node, the three numbers and the creation its image encodes —
//! plus the rule that an image the decoder rejects keeps the C's 3-tuple rather than
//! fabricating a fourth element.
//!
//! # 2. The snapshot rule
//!
//! [`PortRegistry::drain_io_completions`](crate::port::PortRegistry) binds each packet's
//! recipient while the port's metadata is still readable, because a completion staged around
//! a teardown (`async_error_am_all`, `:2597-2608`) outlives that metadata. The tests drive a
//! real registered port whose driver stages a completion from its `output` callback and from
//! its `stop`, so both halves of the rule are exercised through production code:
//! a live port's completion takes its owner/stream from the metadata, a closed one from the
//! close's [`ClosedPortSnapshot`], and a snapshot naming a **different** port never adopts
//! one (a recycled number cannot inherit a dead port's messages).
//!
//! Nothing here fabricates a port lifecycle: the fixture driver registers no descriptor, so
//! `start`/`output`/`stop` are reached by the runtime's own call sites and the port is torn
//! down by [`PortRegistry::close`](crate::port::PortRegistry) like any other.

use beam_io::{CompletionEvent, Driver, DriverContext, InetAsyncResult, InetReason, InetReply};

use super::completion::{ClosedPortSnapshot, CompletionPacket};
use super::*;
use crate::config::RuntimeConfig;

// -----------------------------------------------------------------------------------------------
// Helpers shared by both halves
// -----------------------------------------------------------------------------------------------

/// The arity of a tuple the builders just produced.
fn arity(term: Eterm) -> usize {
    terms::tuple_arity(term).unwrap_or_else(|error| panic!("the arity of a built tuple: {error}"))
}

/// Element `index` (0-based) of a tuple the builders just produced.
fn element(term: Eterm, index: usize) -> Eterm {
    terms::tuple_element(term, index)
        .unwrap_or_else(|error| panic!("element {index} of a built tuple: {error}"))
}

// -----------------------------------------------------------------------------------------------
// Half 1: the builder/render matrix
// -----------------------------------------------------------------------------------------------

/// One runtime, one port, one owner — the arena every builder test builds its terms on.
///
/// The runtime outlives every message: a term is only meaningful while the heap it was built
/// on is alive, so the fixture is created once per test and borrowed for each message (the
/// same shape `src/port/stream_shape_tests.rs` uses).
struct Fixture {
    /// The runtime whose `-eval` arena the messages are built on and whose printer renders
    /// them.
    rt: Runtime,
    /// The port every message is addressed to.
    port: PortId,
    /// The process a completion's packet names (the receipt points of the drain half prove the
    /// *bindings*; here the owner travels straight through).
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

    /// A completion packet for this fixture's port, addressed to its owner.
    fn packet(&self, event: CompletionEvent, stream: StreamType) -> CompletionPacket {
        CompletionPacket {
            port: self.port,
            owner: self.owner,
            stream,
            event,
        }
    }

    /// An `{inet_reply, …}` packet with `caller_ref` as the caller's tag image.
    fn reply(&self, caller_ref: Option<Vec<u8>>, reply: InetReply) -> CompletionPacket {
        self.packet(
            CompletionEvent::InetReply { caller_ref, reply },
            StreamType::Binary,
        )
    }

    /// An `{inet_async, …}` packet.
    fn asynchronous(
        &self,
        id: u16,
        result: InetAsyncResult,
        stream: StreamType,
    ) -> CompletionPacket {
        self.packet(CompletionEvent::InetAsync { id, result }, stream)
    }

    /// The message [`RuntimeContext::completion_message`] builds for `packet`.
    fn message(&mut self, packet: &CompletionPacket) -> Eterm {
        let mut ctx = RuntimeContext::new(&mut self.rt);
        ctx.completion_message(packet)
            .unwrap_or_else(|error| panic!("a completion message for {packet:?}: {error}"))
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

    /// Assert `term` is the atom `name` — the shape every element of both message families
    /// carries.
    fn assert_atom(&self, what: &str, term: Eterm, name: &str) {
        assert_eq!(
            self.atom_name(term).as_deref(),
            Some(name),
            "{what} must be the atom `{name}`"
        );
    }

    /// The `R` of an `{inet_async, Port, Id, R}` message built for `packet`.
    fn answer(&mut self, packet: &CompletionPacket) -> Eterm {
        let message = self.message(packet);
        element(message, 3)
    }

    /// The `Data` of an `{inet_async, Port, Id, {ok, Data}}` message built for `packet`.
    fn async_data(&mut self, packet: &CompletionPacket) -> Eterm {
        element(self.answer(packet), 1)
    }
}

/// The reference id the caller-tag matrix encodes (the third wire number, so the image carries a
/// non-zero `n2` rather than a degenerate all-zero identity).
const TAG_ID: u64 = 1 << 45;

/// The `term_to_binary(Mref, [local])` image `prim_inet:send/3` prefixes its command with.
///
/// Built the way the runtime's own `term_to_binary/2` builds it: a real reference
/// ([`beam_codec::make_internal_ref`]) encoded with the node identity installed, which is the
/// canonical three-number `NEWER_REFERENCE_EXT` (`90`) image — never hand-rolled bytes, so the
/// suite pins the builder against the codec the runtime really uses.
fn tag_image(id: u64) -> Vec<u8> {
    let mut atoms = AtomTable::new();
    let node = atoms.intern("nonode@nohost");
    let mut heap = Heap::new(1 << 10).expect("a scratch heap");
    let reference = beam_codec::make_internal_ref(&mut heap, id).expect("a reference");
    let mut encoder = beam_codec::Encoder::new(&atoms).with_local_node(node, 0);
    encoder
        .encode(reference)
        .expect("the reference's `[local]` image encodes");
    encoder.finish()
}

/// `image` decoded on its own heap and atom table — **the reference the bytes encode**, decoded
/// through the same `beam_codec` entry point the builder uses.
///
/// The heap is handed back so the term stays alive for the caller's assertions.
fn decoded_reference(image: &[u8]) -> (Heap, AtomTable, Eterm) {
    let mut heap = Heap::new(1 << 12).expect("a scratch heap");
    let mut atoms = AtomTable::new();
    let term = beam_codec::decode_term(image, &mut heap, &mut atoms)
        .unwrap_or_else(|error| panic!("the caller-tag image {image:?} decodes: {error}"));
    (heap, atoms, term)
}

/// The `(node name, numbers, creation)` a **wire** reference term carries — its whole identity.
///
/// The decoded caller tag is the C's external reference (it names a node and three numbers); the
/// internal on-heap M1 form is a different representation, so the two are asserted separately
/// below rather than conflated.
fn reference_parts(term: Eterm, atoms: &AtomTable) -> (Option<String>, Vec<u32>, u32) {
    let (node, ids, creation) = beam_codec::term::external_ref_parts(term)
        .unwrap_or_else(|error| panic!("a decoded wire reference: {error}"));
    (
        atoms.name(atom_val(node)).map(str::to_string),
        ids.to_vec(),
        creation,
    )
}

/// A successful `erlang:port_command/2` is the C's **3-tuple** `{inet_reply, Port, ok}`
/// (`inet_reply_ok` → `inet_reply_finish` with no caller tag, `inet_drv.c:2614-2692`), which is
/// what `prim_inet:send/4` matches on.
#[test]
fn an_ok_reply_is_the_three_tuple_with_the_port_and_the_ok_atom() {
    let mut fixture = Fixture::new();
    let packet = fixture.reply(None, InetReply::Ok);
    let message = fixture.message(&packet);
    assert_eq!(arity(message), 3, "a tagless reply is the C's 3-tuple");
    fixture.assert_atom("element 1", element(message, 0), "inet_reply");
    assert_eq!(
        element(message, 1),
        fixture.port.to_eterm(),
        "element 2 is the port the command went to"
    );
    fixture.assert_atom("element 3", element(message, 2), "ok");
}

/// A failed command is `{inet_reply, Port, {error, Reason}}`, the reason rendered as the atom the
/// inet driver's `error_atom`/`am_closed`/`am_timeout` produce (`inet_reply_error_am`,
/// `inet_drv.c:2719-2741`).
///
/// `InetReason::atom_name` names an errno through **`beam_io::error::errno_atom`**, so the
/// unnamed-errno fallback is asserted too: a reason is never silently dropped. (`ECONNREFUSED`
/// is *not* in that table — it is `errno_111` here — which is why a named errno is used below;
/// the inet driver's own `wire.rs` table is a different one.)
#[test]
fn a_failed_reply_carries_the_reasons_error_atom() {
    let mut fixture = Fixture::new();
    for (reason, expected) in [
        (InetReason::Closed, "closed"),
        (InetReason::Timeout, "timeout"),
        (InetReason::from_errno(libc::EPIPE), "epipe"),
        (InetReason::from_errno(libc::ETIMEDOUT), "etimedout"),
        (InetReason::from_errno(4711), "errno_4711"),
    ] {
        let packet = fixture.reply(None, InetReply::Error(reason));
        let message = fixture.message(&packet);
        assert_eq!(arity(message), 3, "{expected}: a tagless reply is the 3-tuple");
        fixture.assert_atom("element 1", element(message, 0), "inet_reply");
        assert_eq!(element(message, 1), fixture.port.to_eterm());
        let status = element(message, 2);
        assert_eq!(arity(status), 2, "{expected}: the reason is the `{{error, R}}` pair");
        fixture.assert_atom("the pair's tag", element(status, 0), "error");
        fixture.assert_atom("the pair's reason", element(status, 1), expected);
    }
}

/// A command that carried a caller tag delivers the C's **4-tuple**, whose fourth element is the
/// tag *decoded* (`inet_reply_finish`, `inet_drv.c:2614-2637`) — the reference
/// `prim_inet:send/4` matches its own `Mref` against.
///
/// The identity is asserted against an **independent decode of the very same image**, and a
/// second, different reference is shown to disagree, so the assertion cannot pass on "some
/// reference".
#[test]
fn a_caller_tag_appends_the_decoded_reference_as_a_fourth_element() {
    let mut fixture = Fixture::new();
    let image = tag_image(TAG_ID);
    let packet = fixture.reply(Some(image.clone()), InetReply::Ok);
    let message = fixture.message(&packet);
    assert_eq!(arity(message), 4, "a tagged command's reply is the 4-tuple");
    fixture.assert_atom("element 1", element(message, 0), "inet_reply");
    assert_eq!(element(message, 1), fixture.port.to_eterm());
    fixture.assert_atom("element 3", element(message, 2), "ok");
    let tag = element(message, 3);
    let (_expected_heap, expected_atoms, expected) = decoded_reference(&image);
    let ours = reference_parts(tag, fixture.rt.atoms());
    assert_eq!(
        ours,
        reference_parts(expected, &expected_atoms),
        "element 4 is the reference the `[local]` image encodes"
    );
    assert_eq!(
        ours.1,
        vec![0, 0, 8192],
        "the id `1 << 45` sits in the third wire number"
    );
    assert_eq!(
        ours.0.as_deref(),
        Some("nonode@nohost"),
        "the image names the node it was encoded with"
    );
    // Teeth: a *different* reference must not compare equal, so the identity is the image's.
    let other_image = tag_image(0x2A);
    let (_other_heap, other_atoms, other) = decoded_reference(&other_image);
    let theirs = reference_parts(other, &other_atoms);
    assert_ne!(
        ours.1, theirs.1,
        "the tag's numbers are the image's, not any reference's"
    );
    assert_ne!(ours, theirs, "a different caller tag is a different element");
    assert!(
        beam_codec::internal_ref_id(tag).is_err(),
        "a wire reference decodes to the C's external form, not the internal M1 one"
    );
}

/// A caller tag the decoder **rejects** keeps the C's 3-tuple (`inet_reply_caller_ref`'s
/// `undefined` arm, `inet_drv.c:2642-2664`) and never costs the owner the reply itself: the
/// status is still the command's real one.
#[test]
fn an_undecodable_caller_tag_keeps_the_three_tuple_and_the_reply() {
    let mut fixture = Fixture::new();
    for (reply, expected_error) in [
        (InetReply::Ok, None),
        (InetReply::Error(InetReason::Closed), Some("closed")),
    ] {
        let packet = fixture.reply(Some(vec![131, 0xFF]), reply);
        let message = fixture.message(&packet);
        assert_eq!(
            arity(message),
            3,
            "an undecodable tag keeps the 3-tuple, never a fabricated fourth element"
        );
        fixture.assert_atom("element 1", element(message, 0), "inet_reply");
        assert_eq!(element(message, 1), fixture.port.to_eterm());
        let status = element(message, 2);
        match expected_error {
            None => fixture.assert_atom("the reply is still `ok`", status, "ok"),
            Some(reason) => {
                fixture.assert_atom("the reply is still `{error, …}`", element(status, 0), "error");
                fixture.assert_atom("with the driver's reason", element(status, 1), reason);
            }
        }
    }
}

/// The `{inet_async, Port, Id, Result}` family: the `Id` is the bare **small integer** the
/// control op handed out (the C's `int Ref`), and the three scalar results are `ok`,
/// `{ok, Port2}` and `{error, Reason}` (`send_async_ok`/`_ok_port`/`_error`,
/// `inet_drv.c:2493-2562`).
#[test]
fn the_async_scalar_results_render_the_cs_fourth_element() {
    let mut fixture = Fixture::new();
    let id = 4321u16;
    for (result, label) in [
        (InetAsyncResult::Ok, "ok"),
        (InetAsyncResult::Error(InetReason::Closed), "{error, closed}"),
        (InetAsyncResult::Port(PortId::new(41)), "{ok, Port2}"),
    ] {
        let packet = fixture.asynchronous(id, result, StreamType::Binary);
        let message = fixture.message(&packet);
        // The envelope, the same for all three: `{inet_async, Port, Id, _}`.
        assert_eq!(arity(message), 4, "{label}: the C's 4-tuple");
        fixture.assert_atom("element 1", element(message, 0), "inet_async");
        assert_eq!(element(message, 1), fixture.port.to_eterm());
        assert_eq!(
            element(message, 2),
            make_small(i64::from(id)),
            "{label}: the id is the bare small integer the control op handed out"
        );
        // ...and the answer itself.
        let answer = element(message, 3);
        match label {
            "ok" => fixture.assert_atom("`send_async_ok`'s answer", answer, "ok"),
            "{error, closed}" => {
                assert_eq!(arity(answer), 2);
                fixture.assert_atom("the pair's tag", element(answer, 0), "error");
                fixture.assert_atom(
                    "`async_error_am`'s reason",
                    element(answer, 1),
                    "closed",
                );
            }
            _ => {
                assert_eq!(arity(answer), 2);
                fixture.assert_atom("the pair's tag", element(answer, 0), "ok");
                assert_eq!(
                    element(answer, 1),
                    PortId::new(41).to_eterm(),
                    "`send_async_ok_port`'s second port"
                );
            }
        }
    }
}

/// A UDP datagram in `[binary]` mode is the C's **improper** `[AF, PortHi, PortLo, A, B, C, D |
/// <<Payload>>]`, and in `[list]` mode one flat integer list with the packed address first
/// (`inet_async_data`'s `hsz > 0` branch, `inet_drv.c:3162`; the header is
/// `pack_address`'s `[AF_INET, ntohs(dport) >> 8, ntohs(dport), a, b, c, d]`).
///
/// Every header byte is delivered **verbatim** — the 16-bit port field is the two bytes
/// `31, 144` (`0x1F90` = 8080), which `prim_inet` re-parses when it builds `{Address, Port,
/// Data}`. The emulator never splits it.
#[test]
fn a_udp_datagram_is_the_packed_address_header_then_the_payload() {
    let mut fixture = Fixture::new();
    let read = ReadMessage {
        header: vec![1, 0x1F, 0x90, 127, 0, 0, 1],
        body: Some(b"hi".to_vec()),
    };
    // The default `list` stream: one flat char list, address first.
    let packet = fixture.asynchronous(5, InetAsyncResult::Data(read.clone()), StreamType::List);
    let data = fixture.async_data(&packet);
    assert!(
        terms::list_elements(data).is_ok(),
        "a `[list]` socket's datagram is one proper list"
    );
    assert_eq!(
        fixture.render(data),
        "[1,31,144,127,0,0,1,104,105]",
        "the packed address bytes then the payload, as char integers"
    );
    assert_eq!(
        terms::cons_head(data).expect("the flat datagram list's first cell"),
        make_small(1),
        "the AF_INET byte leads the header"
    );
    // The `[binary]` stream: the header bytes consed on the payload binary (improper).
    let packet = fixture.asynchronous(5, InetAsyncResult::Data(read), StreamType::Binary);
    let data = fixture.async_data(&packet);
    assert!(
        terms::list_elements(data).is_err(),
        "a `[binary]` socket's datagram is an improper list ending in the payload"
    );
    assert_eq!(
        fixture.render(data),
        "[1,31,144,127,0,0,1|<<104,105>>]",
        "the header bytes consed on the payload binary"
    );
    let mut tail = data;
    for _ in 0..7 {
        tail = terms::cons_tail(tail).expect("the address header's cons cells");
    }
    assert_eq!(
        terms::binary_bytes(tail).expect("the payload is the improper list's tail"),
        b"hi",
        "the datagram itself rides the tail binary"
    );
}

/// A TCP read carries no address: an empty header with a body is the bare payload — a binary on
/// a `[binary]` port and a char list on the default one (`inet_async_data`'s `hsz == 0` branch).
#[test]
fn a_tcp_read_is_the_bare_binary_or_the_char_list() {
    let mut fixture = Fixture::new();
    let read = ReadMessage::body_only(b"hi".to_vec());
    let packet = fixture.asynchronous(6, InetAsyncResult::Data(read.clone()), StreamType::Binary);
    let data = fixture.async_data(&packet);
    assert_eq!(
        terms::binary_bytes(data).expect("a `[binary]` TCP read is one binary"),
        b"hi",
        "the payload is the bare binary, not a list"
    );
    assert_eq!(fixture.render(data), "<<104,105>>");
    let packet = fixture.asynchronous(6, InetAsyncResult::Data(read), StreamType::List);
    let data = fixture.async_data(&packet);
    assert!(
        terms::list_elements(data).is_ok(),
        "the default stream renders the same bytes as a char list"
    );
    assert_eq!(fixture.render(data), "[104,105]");
}

// -----------------------------------------------------------------------------------------------
// Half 2: the drain's owner/stream snapshot
// -----------------------------------------------------------------------------------------------

/// The pid every registry-level fixture port belongs to.
const OWNER: ProcessId = ProcessId::new(11, 0);

/// A second pid, so a completion attributed to the wrong recipient is visible.
const OTHER_OWNER: ProcessId = ProcessId::new(12, 0);

/// `{inet_async, Port, id, ok}` — a completion whose `Id` identifies which driver callback
/// staged it.
fn async_ok(id: u16) -> CompletionEvent {
    CompletionEvent::InetAsync {
        id,
        result: InetAsyncResult::Ok,
    }
}

/// A driver that stages one completion from `output` (a command's `{inet_reply, …}`) and/or one
/// from its teardown (the C's `async_error_am_all` flush of every queued operation).
///
/// It registers no descriptor, holds no child and arms no timer, so every callback below is
/// reached by the runtime's own call sites — there is nothing to poll and nothing to wait for.
struct StageDriver {
    /// What the `output` callback hands up, once.
    reply: Option<CompletionEvent>,
    /// What the teardown hands up, once.
    teardown: Option<CompletionEvent>,
}

impl StageDriver {
    /// A driver that stages `event` from `output` — the `port_command/2` reply.
    fn replying(event: CompletionEvent) -> StageDriver {
        StageDriver {
            reply: Some(event),
            teardown: None,
        }
    }

    /// A driver that stages `event` while it is torn down — the `async_error_am_all` flush.
    fn on_teardown(event: CompletionEvent) -> StageDriver {
        StageDriver {
            reply: None,
            teardown: Some(event),
        }
    }
}

impl Driver for StageDriver {
    fn name(&self) -> &str {
        "completion_fixture"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        // Consume what the command queued (a driver's `output` is the write) and then hand the
        // reply up, the order a real socket driver uses.
        let written = ctx.output().len();
        ctx.output().consume(written);
        match self.reply.take() {
            Some(event) => {
                ctx.deliver_completion(event)?;
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
            Some(event) => ctx.deliver_completion(event),
            None => Ok(()),
        }
    }
}

/// The `port_info` metadata of a `{spawn_driver, _}` port owned by `owner` on `stream` — the
/// block `open_port/2`'s driver arm builds, without a child, a descriptor or a `Name` decode.
fn port_meta(owner: ProcessId, stream: StreamType) -> PortMeta {
    PortMeta {
        command: "completion_fixture".to_string(),
        os_pid: None,
        stream,
        line: None,
        packet: None,
        notify_exit: false,
        monitors: Vec::new(),
        owner,
        // The `Name` form whose bytes come from the driver's own `output` callback (see
        // `PortName::bytes_from_driver_output`), which is the arm a socket driver takes.
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

/// A registry holding `driver` as a **live** port owned by `owner` on `stream`.
///
/// The id is a real one — [`PortRegistry::register`] allocates it from `beam_io`'s registry, so
/// `contains`, `close` and the metadata all agree.
fn registry_with(
    driver: StageDriver,
    owner: ProcessId,
    stream: StreamType,
) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new().expect("a port registry");
    let id = registry
        .register(Box::new(driver), port_meta(owner, stream))
        .expect("the fixture port registers");
    (registry, id)
}

/// The completion packets among `events`, in report order.
fn completions(events: Vec<PortEvent>) -> Vec<CompletionPacket> {
    events
        .into_iter()
        .filter_map(|event| match event {
            PortEvent::Completion { packet } => Some(packet),
            _ => None,
        })
        .collect()
}

/// A live port's completion is bound to that port's **owner** and **stream** at drain time.
///
/// This is the ordinary path: `erlang:port_command/2` runs the driver's `output` callback
/// (which stages the reply) and [`PortRegistry::write_command`] drains the channel straight
/// after, so the packet names the recipient while the metadata is still readable.
#[test]
fn a_live_ports_completion_is_bound_to_its_owner_and_stream_at_drain_time() {
    let (mut registry, id) = registry_with(
        StageDriver::replying(async_ok(5)),
        OWNER,
        StreamType::Binary,
    );
    registry
        .write_command(id, b"hi")
        .expect("the command reaches the driver");
    let packets = completions(registry.drain_pending_events());
    assert_eq!(packets.len(), 1, "exactly one completion, reported once");
    assert_eq!(packets[0].port, id, "the port the driver belongs to");
    assert_eq!(packets[0].owner, OWNER, "the drain binds the port's owner");
    assert_eq!(
        packets[0].stream,
        StreamType::Binary,
        "and the port's stream option, which shapes a `Data` payload"
    );
    assert_eq!(packets[0].event, async_ok(5), "the driver's own completion");
}

/// A completion staged **around a close** still reaches its owner: the metadata is gone by the
/// time the drain sees it, so the owner and stream come from the close's
/// [`ClosedPortSnapshot`] — the C's `async_error_am_all` rule, which is what keeps a blocked
/// `gen_tcp:recv` from waiting for ever on a socket that closed under it.
#[test]
fn a_closed_ports_teardown_completion_still_reaches_its_owner() {
    let event = CompletionEvent::reply_error(None, InetReason::Closed);
    let (mut registry, id) = registry_with(
        StageDriver::on_teardown(event.clone()),
        OWNER,
        StreamType::List,
    );
    registry.close(id).expect("the port closes");
    assert!(
        !registry.contains(id),
        "the close removed the metadata a drain would otherwise read"
    );
    let packets = completions(registry.drain_pending_events());
    assert_eq!(
        packets.len(),
        1,
        "the teardown's flush is reported exactly once"
    );
    assert_eq!(packets[0].port, id);
    assert_eq!(
        packets[0].owner, OWNER,
        "the recipient came from the close's snapshot, not from metadata that is gone"
    );
    assert_eq!(
        packets[0].stream,
        StreamType::List,
        "so did the stream option"
    );
    assert_eq!(
        packets[0].event, event,
        "and the reason is the driver's `{{error, closed}}`"
    );
}

/// A snapshot is consulted for **its own** port only: a completion whose port has no metadata
/// and whose snapshot names a different port is dropped, never attributed to that other port's
/// owner — so a **recycled** port number cannot inherit a dead port's messages.
///
/// The two halves use the same fixture: the completion is harvested at `beam_io` level exactly
/// as [`PortRegistry::close`] harvests it, and the runtime metadata is removed by hand — the
/// state a drain sees when a completion outlives both its port and the snapshot of that close.
#[test]
fn a_snapshot_attributes_only_its_own_ports_completion() {
    // (a) The port's **own** snapshot resolves it.
    let (mut registry, id) = registry_with(
        StageDriver::on_teardown(async_ok(3)),
        OWNER,
        StreamType::Binary,
    );
    {
        let PortRegistry {
            registry: io,
            poller,
            meta,
            ..
        } = &mut registry;
        io.close(poller, id).expect("beam_io closes the port");
        meta.remove(&id.number());
    }
    let own = ClosedPortSnapshot {
        port: id,
        owner: OWNER,
        stream: StreamType::Binary,
        line: None,
    };
    let packets = completions(registry.drain_io_completions(Some(own)));
    assert_eq!(
        packets.len(),
        1,
        "the close's snapshot resolves the completion staged around it"
    );
    assert_eq!(packets[0].port, id);
    assert_eq!(packets[0].owner, OWNER);
    assert_eq!(packets[0].stream, StreamType::Binary);

    // (b) A snapshot naming **another** port does not: the completion has no recoverable
    // recipient and is dropped — and consumed, so it is never re-reported against a later port
    // that reuses the number.
    let (mut registry, id) = registry_with(
        StageDriver::on_teardown(async_ok(4)),
        OWNER,
        StreamType::Binary,
    );
    {
        let PortRegistry {
            registry: io,
            poller,
            meta,
            ..
        } = &mut registry;
        io.close(poller, id).expect("beam_io closes the port");
        meta.remove(&id.number());
    }
    let stale = ClosedPortSnapshot {
        port: PortId::new(id.number() + 1),
        owner: OTHER_OWNER,
        stream: StreamType::List,
        line: None,
    };
    assert!(
        registry.drain_io_completions(Some(stale)).is_empty(),
        "a snapshot for another port must not adopt this completion"
    );
    assert!(
        registry.drain_io_completions(None).is_empty(),
        "and the drop is final: the completion was never re-reported against a reused number"
    );
}

/// A completion the drain has staged but no pump has reported yet keeps the runtime **polling**:
/// it is a message its owner is still owed, so [`PortRegistry::has_nothing_to_poll`] answers
/// `false` until a pump has handed it over.
///
/// This is what makes a closed port's `{error, closed}` deliverable on the scheduler route — a
/// runtime that decided "nothing is live, nothing to poll" would strand it.
#[test]
fn a_buffered_completion_keeps_the_runtime_polling() {
    let mut registry = PortRegistry::new().expect("a port registry");
    assert!(
        registry.has_nothing_to_poll(),
        "an empty registry has nothing to wait on"
    );
    let packet = CompletionPacket {
        port: PortId::new(3),
        owner: OWNER,
        stream: StreamType::Binary,
        event: async_ok(2),
    };
    registry.pending_events.push(PortEvent::Completion { packet });
    assert!(
        !registry.has_nothing_to_poll(),
        "a buffered completion is pending work, not an idle port"
    );
    let packets = completions(registry.drain_pending_events());
    assert_eq!(packets.len(), 1, "the pump reports it exactly once");
    assert_eq!(packets[0].owner, OWNER);
    assert!(
        registry.has_nothing_to_poll(),
        "and only once it has been reported is there nothing left to poll for"
    );
}
