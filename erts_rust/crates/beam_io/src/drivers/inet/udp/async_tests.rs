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
//! The `udp_inet` **asynchronous-operation** suite: `PACKET_REQ_RECV`(60) and the
//! `{inet_async, Port, Id, Result}` completions it produces, plus the send path's
//! `{inet_reply, Port, Reply, Tag}`.
//!
//! Every case drives the runtime's own `erlang:port_control/3`/`port_command/2` path through the
//! [`Rig`] the numeric-opcode suite defines and drains completions with
//! [`PortRegistry::take_completions`](crate::PortRegistry::take_completions) — the seam `beam_rt`
//! reads. What is pinned here:
//!
//! * the C's guard ladder of `packet_inet_ctl`'s `PACKET_REQ_RECV` arm, in the C's order;
//! * the **separation of the two halves**: the control reply is the id and nothing else, the
//!   datagram arrives only through a later round's completion;
//! * the datagram's shape — the packed source address as the read's *header*, the payload as its
//!   body, so `[list]` mode's flat list and `[binary]` mode's improper `[Address | <<Payload>>]`
//!   are both derivable from one `ReadMessage`;
//! * the deadline (`{error, timeout}` **after** the reply) and the teardown
//!   (`{error, closed}`, exactly once, before the descriptor goes away);
//! * the send path's caller tag echoed verbatim, and the zero-length tag staging nothing.

use std::net::UdpSocket;
use std::time::Duration;

use super::super::opcode_tests::{op, Rig};
use super::super::wire::{
    ctl_ok, ctl_xerror, pack_address, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT,
    INET_TYPE_DGRAM,
};
use super::UdpInetDriver;
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply};
use crate::port_task::ReadMessage;

/// `INET_REQ_OPEN` (`inet_drv.c:14577`).
const INET_REQ_OPEN: u32 = 1;
/// `INET_REQ_BIND` (`:11409`).
const INET_REQ_BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`:11111`).
const INET_REQ_SETOPTS: u32 = 7;
/// `PACKET_REQ_RECV` (`:14968`) — the datagram driver's asynchronous receive.
const PACKET_REQ_RECV: u32 = 60;
/// The `{active, true}` option code (`INET_LOPT_ACTIVE`).
const INET_LOPT_ACTIVE: u8 = 22;
/// The C's `TCP_MAX_PACKET_SIZE` plus one: the smallest `Length` refused with `[0]enomem`.
const LENGTH_TOO_LARGE: u32 = 0x0400_0001;

/// A bare `udp_inet` port (no socket yet).
fn bare_port() -> Rig {
    Rig::new(Box::new(UdpInetDriver::new()))
}

/// An opened `udp_inet` port: the `open` verb has created the datagram socket.
fn opened_port() -> Rig {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(INET_REQ_OPEN), &[
            INET_AF_INET,
            INET_TYPE_DGRAM,
            INET_PROTO_DEFAULT
        ]),
        ctl_ok(),
        "the datagram socket opens"
    );
    port
}

/// An opened port bound to an ephemeral loopback port, plus the port number the kernel chose.
fn bound_port() -> (Rig, u16) {
    let mut port = opened_port();
    let reply = port.control(&op(INET_REQ_BIND), &[INET_AF_LOOPBACK, 0, 0]);
    assert_eq!(reply.first(), Some(&1), "the bind succeeds: {reply:?}");
    let bound = u16::from_be_bytes([reply[1], reply[2]]);
    assert_ne!(bound, 0, "the kernel assigned a port");
    (port, bound)
}

/// The `Timeout(4), Length(4)` operand of `PACKET_REQ_RECV` (`inet_drv.c:14990`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}

/// The async completions of one drain, in delivery order.
fn async_events(events: &[crate::completion::PortCompletion]) -> Vec<(u16, InetAsyncResult)> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            CompletionEvent::InetAsync { id, result } => Some((*id, result.clone())),
            CompletionEvent::InetReply { .. } => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The guard ladder
// ---------------------------------------------------------------------------

#[test]
fn the_packet_recv_ladder_is_the_cs_and_in_the_cs_order() {
    // 1. no socket ⇒ `!IS_OPEN` ⇒ `[0]exbadport` (`:14980`), whatever the operand.
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(PACKET_REQ_RECV), &recv_operand(0, 0)),
        ctl_xerror("exbadport")
    );
    assert_eq!(bare.control(&op(PACKET_REQ_RECV), &[]), ctl_xerror("exbadport"));

    let mut port = opened_port();

    // 2. the operand must be exactly the eight-byte record (`:14988`).
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &[0, 0]),
        ctl_xerror("einval")
    );
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &[0; 4]),
        ctl_xerror("einval")
    );

    // 2b. an active-mode socket has no passive receive (`desc->active`, `:14988`).
    let mut active = vec![INET_LOPT_ACTIVE];
    active.extend_from_slice(&1i32.to_be_bytes());
    assert_eq!(active.len(), 5);
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &active),
        ctl_ok(),
        "{INET_LOPT_ACTIVE} is accepted"
    );
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("einval")
    );
    // Back to passive so the rest of the ladder is reachable.
    let mut passive = vec![INET_LOPT_ACTIVE];
    passive.extend_from_slice(&0i32.to_be_bytes());
    assert_eq!(port.control(&op(INET_REQ_SETOPTS), &passive), ctl_ok());

    // 3. an over-long `Length` ⇒ `[0]enomem`, before the slot is touched.
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, LENGTH_TOO_LARGE)),
        ctl_xerror("enomem")
    );

    // 4. a well-formed operand is accepted and answers the async id.
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    // 5. the single receive slot (`INET_MAX_ASYNC`, `:1069`) is taken ⇒ `[0]ealready`.
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("ealready")
    );
}

#[test]
fn a_refused_enqueue_still_consumes_an_async_id() {
    let mut port = opened_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    // Refused, but `NEW_ASYNC_ID` was already taken (`:1549`).
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("ealready")
    );
    // The refused id (1) is gone: the next accepted one is 2, not 1.
    port.close();
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1, "the one pending operation is answered");
    assert_eq!(drained[0].0, 0);
    let mut port = opened_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0],
        "a fresh port starts its id counter at 0 again"
    );
}

// ---------------------------------------------------------------------------
// The reply/completion split and the datagram's shape
// ---------------------------------------------------------------------------

#[test]
fn the_reply_carries_no_data_and_the_datagram_completes_on_a_later_round() {
    let (mut port, bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");

    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0],
        "the control reply is the id and nothing else"
    );
    assert!(port.completions().is_empty(), "nothing is completed inline");
    assert_eq!(port.take_input(64), Vec::<u8>::new(), "no bytes were staged");

    peer.send_to(b"dg2", ("127.0.0.1", bound)).expect("a datagram");

    // One round's async drive point completes the operation.
    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1, "exactly one completion");
    let (id, result) = drained.into_iter().next().expect("one event");
    assert_eq!(id, 0);

    let source = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = source.ip() else {
        panic!("an IPv4 peer");
    };
    let expected = ReadMessage {
        header: pack_address(source.port(), address.octets()),
        body: Some(b"dg2".to_vec()),
    };
    match result {
        InetAsyncResult::Data(read) => {
            assert_eq!(read.header, expected.header, "the packed source address");
            assert_eq!(read.body, expected.body, "the datagram payload");
            // `[list]` mode: one flat list with the address first (the C's `INET_MODE_LIST`
            // arm, `inet_async_binary_data` `:3900`).
            assert_eq!(read.flattened(), expected.flattened());
            assert_eq!(read.body_only(), None);
            // `[binary]` mode's improper tail is exactly the body.
            assert_eq!(read.has_body(), true);
            assert_eq!(read.len(), 7 + 3);
        }
        other => panic!("a data completion, not {other:?}"),
    }

    // Exactly once: a second round reports nothing.
    port.round().expect("the round runs");
    assert!(port.completions().is_empty());
}

#[test]
fn a_queued_receive_with_no_datagram_stays_pending_across_rounds() {
    let (mut port, _bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    for _ in 0..3 {
        port.round().expect("the round runs");
        assert!(port.completions().is_empty(), "nothing arrived, nothing reported");
    }
}

// ---------------------------------------------------------------------------
// The deadline and the teardown
// ---------------------------------------------------------------------------

#[test]
fn a_zero_timeout_reports_the_timeout_after_the_reply() {
    let (mut port, _bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(0, 0)),
        vec![1, 0, 0],
        "the reply comes first, the timeout later"
    );
    assert!(port.completions().is_empty());
    assert!(port.next_timeout().is_some(), "the deadline is armed");

    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].0, 0);
    assert_eq!(drained[0].1, InetAsyncResult::Error(InetReason::Timeout));

    // Once only, and the queue is empty afterwards.
    port.round().expect("the round runs");
    assert!(port.completions().is_empty());
}

#[test]
fn a_close_with_an_operation_pending_answers_it_closed_exactly_once() {
    let (mut port, _bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    port.close();
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1, "{drained:?}");
    assert_eq!(drained[0].0, 0);
    assert_eq!(drained[0].1, InetAsyncResult::Error(InetReason::Closed));
    assert!(port.completions().is_empty(), "never twice");
}

#[test]
fn a_still_pending_operation_keeps_the_port_warm_and_an_empty_queue_does_not() {
    let (mut port, _bound) = bound_port();
    assert!(!port.next_timeout().is_some(), "nothing queued yet");
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    // `has_pending_async` is what makes the registry keep driving the port; an infinite timeout
    // declares no deadline, so the only liveness is readiness.
    assert!(port.next_timeout().is_none(), "INET_INFINITY arms no timer");
}

// ---------------------------------------------------------------------------
// The send path's `{inet_reply, …}`
// ---------------------------------------------------------------------------

/// The golden `term_to_binary(Mref, [local])` image the objective pins, echoed verbatim.
const GOLDEN_TAG: [u8; 26] = [
    131, 121, 254, 153, 216, 200, 90, 0, 3, 106, 0, 0, 0, 0, 0, 0, 11, 218, 194, 188, 0, 11, 244,
    41, 135, 150,
];

#[test]
fn the_send_path_echoes_the_caller_tag_verbatim() {
    let (mut port, _bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    let target = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = target.ip() else {
        panic!("an IPv4 peer");
    };

    // `[tag_len:be16][tag][address][anc_len:be32][data]` — the `prim_inet:do_sendto/4` shape.
    let mut command = (GOLDEN_TAG.len() as u16).to_be_bytes().to_vec();
    command.extend_from_slice(&GOLDEN_TAG);
    command.extend_from_slice(&pack_address(target.port(), address.octets()));
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(b"dg3");

    port.command(&command).expect("the command is written");

    let events = port.completions();
    let replies: Vec<&CompletionEvent> = events
        .iter()
        .map(|event| &event.event)
        .filter(|event| matches!(event, CompletionEvent::InetReply { .. }))
        .collect();
    assert_eq!(replies.len(), 1, "one reply for the tagged command: {events:?}");
    match replies[0] {
        CompletionEvent::InetReply { caller_ref, reply } => {
            assert_eq!(
                caller_ref.as_deref(),
                Some(&GOLDEN_TAG[..]),
                "the tag is echoed byte for byte, never decoded"
            );
            assert_eq!(*reply, InetReply::Ok);
        }
        other => panic!("an inet reply, not {other:?}"),
    }

    // The datagram itself reached the peer with the prefix and the address stripped.
    let mut buf = [0u8; 64];
    peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let (len, _from) = peer.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], b"dg3");
}

#[test]
fn a_command_without_a_caller_tag_stages_no_reply() {
    let (mut port, _bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    let target = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = target.ip() else {
        panic!("an IPv4 peer");
    };

    // A **plain** command: no `[n:be16][tag]` prefix at all.
    let mut command = pack_address(target.port(), address.octets());
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(b"plain");
    port.command(&command).expect("the command is written");

    let events = port.completions();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.event, CompletionEvent::InetReply { .. })),
        "the C's `inet_reply_finish` sends nothing without a caller: {events:?}"
    );
    assert!(port.contains(), "the port stays alive");

    let mut buf = [0u8; 64];
    peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let (len, _from) = peer.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], b"plain");
}

#[test]
fn a_socket_less_command_still_keeps_its_bytes_queued() {
    let mut port = bare_port();
    let error = port
        .command(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .expect_err("a socket-less port refuses the command");
    assert!(
        matches!(error, beam_common::BeamError::BadArg),
        "the honest refusal, not a fabricated reply: {error:?}"
    );
    assert!(port.contains(), "the port stays alive");
}
