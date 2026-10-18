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

//! The **accept** state machine of the built-in socket drivers, at the driver boundary: the
//! `INET_REQ_ACCEPT`(26) guard, the multi-op accept queue it feeds, the asynchronous ids it hands
//! out, the state bits `INET_REQ_GETSTATUS`(16) reports for the queued operations, the listener's
//! readiness arming and the teardown drain.
//!
//! Everything here goes through the crate's own [`PortRegistry`](crate::PortRegistry) /
//! [`Poller`](crate::Poller) path — the same [`Rig`] the numeric-opcode suite uses — because the
//! accept reply is only the *id*: the operation's outcome is a
//! [`CompletionEvent::InetAsync`](crate::completion::CompletionEvent::InetAsync) staged on a
//! **later** port-task round, which is exactly what a live runtime drains.
//!
//! The suite is fully **hermetic**: no `erl` oracle, no connecting peer and no sleeps. Every
//! listener binds `127.0.0.1:0` (so the kernel picks an ephemeral port and nothing is exposed) and
//! nothing ever connects to it, which is what keeps the accept queue's *outcomes* deterministic:
//! a long deadline never fires, a zero deadline fires on the next round, and a close drains what
//! is left. The half that needs a real peer — a completed accept handing over an
//! [`InetAsyncResult::Accepted`](crate::completion::InetAsyncResult) descriptor, a peer close, and
//! a single port holding both a pending accept and a pending recv — lives in the sibling
//! real-socket suites (`tests/inet_async.rs`, `async_tests.rs`), never here.
use super::op_queue::async_ok_reply;
use super::opcode_tests::{loopback_arg, op, open_arg, Rig};
use super::wire::{ctl_ok, ctl_xerror, getstatus_reply};
use super::{
    make_tcp_inet, make_udp_inet, INET_AF_INET, INET_LOPT_ACTIVE, INET_PROTO_DEFAULT, INET_REP_OK,
    INET_TYPE_DGRAM,
};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason};

/// `INET_REQ_OPEN` (`inet_drv.c:11961`).
const OPEN: u32 = 1;
/// `INET_REQ_CLOSE` (`inet_drv.c:11887`).
const CLOSE: u32 = 2;
/// `INET_REQ_BIND` (`inet_drv.c:11409`).
const BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`inet_drv.c:6820`).
const SETOPTS: u32 = 7;
/// `INET_REQ_GETOPTS` (`inet_drv.c:9127`).
const GETOPTS: u32 = 8;
/// `INET_REQ_GETSTATUS` (`inet_drv.c:11148`).
const GETSTATUS: u32 = 16;
/// `INET_REQ_ACCEPT` (`inet_drv.c:12144`).
const ACCEPT: u32 = 26;
/// `INET_REQ_LISTEN` (`inet_drv.c:12042`).
const LISTEN: u32 = 27;

/// `INET_STATE_LISTENING` — a listener with **no** accept queued: `OPEN | INET_F_LISTEN` = `1 | 8`.
const LISTENING_STATUS: u32 = 9;
/// One queued accept: `9 | INET_F_ACC` (`0x0020`) = `0x29`. Measured on the oracle.
const ONE_ACCEPT_STATUS: u32 = 41;
/// Two or more queued accepts: `9 | INET_F_ACC | INET_F_MULTI_CLIENT` (`0x0100`) = `0x129`.
/// Measured on the oracle.
const MULTI_ACCEPT_STATUS: u32 = 297;

/// A 60 000 ms accept deadline: far enough away that no round can reach it, so the queue stays put
/// while the state bits are inspected.
const LONG_TIMEOUT: [u8; 4] = [0, 0, 0xea, 0x60];

/// A zero accept deadline — the C's `driver_set_timer(port, 0)`, due at the next round.
const ZERO_TIMEOUT: [u8; 4] = [0, 0, 0, 0];

/// Asserts a `bind` reply is the C's `[1] ++ port_hi ++ port_lo` and returns the bound port.
fn bound_port(reply: &[u8]) -> u16 {
    assert_eq!(
        reply.len(),
        3,
        "`bind` answers `[1, port_hi, port_lo]`, got {reply:?}"
    );
    assert_eq!(
        reply[0], INET_REP_OK,
        "`bind`'s result tag is `INET_REP_OK`, got {reply:?}"
    );
    u16::from_be_bytes([reply[1], reply[2]])
}

/// Opens, binds `127.0.0.1:0` (the kernel assigns the port) and listens, returning the bound port.
fn listening(rig: &mut Rig) -> u16 {
    assert_eq!(
        rig.control(&op(OPEN), &open_arg()),
        ctl_ok(),
        "`open` on the stream driver answers `[1]`"
    );
    let port = bound_port(&rig.control(&op(BIND), &loopback_arg(0)));
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    assert_eq!(
        rig.control(&op(LISTEN), &[0, 5]),
        ctl_ok(),
        "`listen` with a `[hi, lo]` backlog answers `[1]`"
    );
    port
}

/// Drains the completions the last round staged, without their port id: every case here has one
/// port, so the id would be the same in every element.
fn events(rig: &mut Rig) -> Vec<CompletionEvent> {
    rig.completions().into_iter().map(|done| done.event).collect()
}

/// The `{inet_async, Port, Id, {error, timeout}}` completion.
fn timed_out(id: u16) -> CompletionEvent {
    CompletionEvent::InetAsync {
        id,
        result: InetAsyncResult::Error(InetReason::Timeout),
    }
}

/// The `{inet_async, Port, Id, {error, closed}}` completion a teardown drains a queued operation
/// with.
fn closed(id: u16) -> CompletionEvent {
    CompletionEvent::InetAsync {
        id,
        result: InetAsyncResult::Error(InetReason::Closed),
    }
}

#[test]
fn accept_on_a_non_listener_answers_einval_in_every_state() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));

    // Never opened: the accept guard is the C's combined "not listening **or** bad operand" test
    // (`inet_drv.c:12160`), and it answers `einval` — never `exbadport` — so a bare port refuses an
    // otherwise well-formed 4-byte operand this way.
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on a never-opened port answers `einval`"
    );

    assert_eq!(
        rig.control(&op(OPEN), &open_arg()),
        ctl_ok(),
        "`open` on the stream driver answers `[1]`"
    );
    // Opened but not listening: still `einval` (measured on the oracle in every non-listening
    // state).
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on an opened, non-listening port answers `einval`"
    );

    // Bound but not listening: `bind` does not move the descriptor out of `INET_STATE_OPEN`.
    let port = bound_port(&rig.control(&op(BIND), &loopback_arg(0)));
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on a bound, non-listening port answers `einval`"
    );

    assert_eq!(
        rig.control(&op(CLOSE), &[]),
        ctl_ok(),
        "`close` answers `[1]`, socket-less too"
    );
    // Closed: `INET_STATE_CLOSED` is not `INET_STATE_LISTENING` either, so the same guard fires.
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on a closed port answers `einval`"
    );
}

#[test]
fn listen_faults_and_the_accept_guard_order() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));

    // `INET_REQ_LISTEN`'s guards run in the C's order: the socket-state guard first, the operand
    // length last (`inet_drv.c:12042`). A socket-less port therefore reports `exbadport` even for a
    // well-formed operand.
    assert_eq!(
        rig.control(&op(LISTEN), &[0, 5]),
        ctl_xerror("exbadport"),
        "`listen` before `open` answers `exbadport` (the state guard comes first)"
    );

    assert_eq!(
        rig.control(&op(OPEN), &open_arg()),
        ctl_ok(),
        "`open` on the stream driver answers `[1]`"
    );
    // A 1-byte and a 3-byte operand are both `len != 2` ⇒ `einval`.
    assert_eq!(
        rig.control(&op(LISTEN), &[5]),
        ctl_xerror("einval"),
        "`listen` with a 1-byte operand answers `einval`"
    );
    assert_eq!(
        rig.control(&op(LISTEN), &[0, 0, 5]),
        ctl_xerror("einval"),
        "`listen` with a 3-byte operand answers `einval`"
    );
    assert_eq!(
        rig.control(&op(LISTEN), &[0, 5]),
        ctl_ok(),
        "`listen` with a 2-byte backlog answers `[1]`"
    );

    // The accept operand is `Timeout(4)` and nothing else: 0, 3 and 5 bytes are all `len != 4`
    // ⇒ `einval` on a listener, the same reply the non-listening guard gives — the C folds both
    // faults into one `if`, so the two are indistinguishable to the caller (measured).
    for operand in [&[][..], &[0, 0, 0][..], &[0, 0, 0, 0, 0][..]] {
        assert_eq!(
            rig.control(&op(ACCEPT), operand),
            ctl_xerror("einval"),
            "accept with a {}-byte operand on a listener answers `einval`",
            operand.len()
        );
    }
    // Only the 4-byte operand queues, and the reply is the id and nothing else.
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(0),
        "a 4-byte operand queues the accept and answers `[1] ++ <<Id:16/big>>`"
    );
}

#[test]
fn datagram_accept_is_exbadport() {
    let mut rig = Rig::new(make_udp_inet().expect("the udp_inet driver"));

    // `INET_REQ_ACCEPT` is a case of `tcp_inet_ctl` only: the datagram driver's switch has no such
    // arm, so the number belongs to neither switch and the C answers `[0]exbadport` (measured in
    // every datagram state).
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("exbadport"),
        "accept on a socket-less udp_inet port answers `exbadport`"
    );

    // The datagram `open` operand names `SOCK_DGRAM` (`INET_TYPE_DGRAM`), the stream shape being
    // refused with `einval`; an open datagram port is no more acceptable.
    assert_eq!(
        rig.control(
            &op(OPEN),
            &[INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
        ),
        ctl_ok(),
        "`open` on the datagram driver answers `[1]`"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("exbadport"),
        "accept on an open udp_inet port answers `exbadport`"
    );
}

#[test]
fn a_zero_timeout_completes_exactly_once_and_clears_the_deadline() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    // The reply is only the id (`inet_drv.c:12209`): the outcome never rides the control reply.
    assert_eq!(
        rig.control(&op(ACCEPT), &ZERO_TIMEOUT),
        async_ok_reply(0),
        "a zero-timeout accept answers its id"
    );
    assert!(
        rig.next_timeout().is_some(),
        "the zero deadline arms the port's single `TIMEOUT` task"
    );

    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        vec![timed_out(0)],
        "a zero deadline completes exactly once with `{{error, timeout}}`"
    );
    assert_eq!(
        rig.next_timeout(),
        None,
        "the completed operation leaves no deadline behind"
    );
    // The list drained, so the descriptor-state bits go back to a bare listener
    // (`tcp_inet_multi_timeout`, `inet_drv.c:12607`-`:12611`).
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(LISTENING_STATUS),
        "a drained accept list reports `INET_STATE_LISTENING` again"
    );

    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        Vec::new(),
        "a second round reports nothing: the operation was completed once"
    );

    // The id counter keeps advancing across operations (the C's `op_ref++ & 0xffff`).
    assert_eq!(
        rig.control(&op(ACCEPT), &ZERO_TIMEOUT),
        async_ok_reply(1),
        "the next queued accept answers the next id"
    );
    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        vec![timed_out(1)],
        "the second zero-timeout accept completes once, with its own id"
    );
}

#[test]
fn several_queued_accepts_answer_their_own_id_and_report_the_accept_bits() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    // `INET_REQ_ACCEPT` uses the C's `enq_multi_op` list, which never refuses (`inet_drv.c:12194`),
    // so repeats answer their own id — never `ealready` — in enqueue order.
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(0),
        "the first queued accept answers id 0"
    );
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(ONE_ACCEPT_STATUS),
        "one queued accept reports `INET_STATE_ACCEPTING` (`9 | 0x20`)"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(1),
        "the second queued accept answers id 1"
    );
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(MULTI_ACCEPT_STATUS),
        "two queued accepts add `INET_F_MULTI_CLIENT` (`9 | 0x120`)"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(2),
        "the third queued accept answers id 2"
    );
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(MULTI_ACCEPT_STATUS),
        "three queued accepts report the same multi-accept bits as two"
    );

    // Nothing connected, so no round can complete one: the long deadline is not due either.
    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        Vec::new(),
        "a listener with no connection and no due deadline completes nothing"
    );

    // The teardown drains every pending accept as `{error, closed}`, in enqueue order, exactly
    // once each (the C's `async_error_am_all(am_closed)`).
    rig.close();
    assert_eq!(
        events(&mut rig),
        vec![closed(0), closed(1), closed(2)],
        "`close` drains the three queued accepts in enqueue order"
    );
    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        Vec::new(),
        "a round after the drain reports nothing"
    );
    assert!(!rig.contains(), "the closed port left the registry");
}

#[test]
fn closing_with_pending_accepts_drains_each_exactly_once() {
    // One pending accept.
    let mut one = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut one);
    assert_eq!(
        one.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(0),
        "one accept queues"
    );
    one.close();
    assert_eq!(
        events(&mut one),
        vec![closed(0)],
        "one pending accept is drained exactly once"
    );
    assert_eq!(
        events(&mut one),
        Vec::new(),
        "the drain is exactly once: a second drain is empty"
    );
    assert!(!one.contains(), "the closed port left the registry");

    // Two pending accepts.
    let mut two = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut two);
    assert_eq!(
        two.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(0),
        "the first accept queues"
    );
    assert_eq!(
        two.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(1),
        "the second accept queues"
    );
    two.close();
    assert_eq!(
        events(&mut two),
        vec![closed(0), closed(1)],
        "two pending accepts are drained in enqueue order"
    );
    assert_eq!(
        events(&mut two),
        Vec::new(),
        "the drain is exactly once: a second drain is empty"
    );
    assert!(!two.contains(), "the closed port belongs to the registry no more");
}

#[test]
fn active_does_not_gate_the_accept() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    // `{active, true}` — the five-byte `[opt, val_be32]` record `INET_REQ_SETOPTS` walks.
    let mut record = vec![INET_LOPT_ACTIVE];
    record.extend_from_slice(&1u32.to_be_bytes());
    assert_eq!(
        rig.control(&op(SETOPTS), &record),
        ctl_ok(),
        "`{{active,true}}` is set on the listener"
    );
    assert_eq!(
        rig.control(&op(GETOPTS), &[INET_LOPT_ACTIVE]),
        {
            let mut reply = ctl_ok();
            reply.push(INET_LOPT_ACTIVE);
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply
        },
        "`getopts` reports the stored `active` value"
    );

    // The C's accept guard (`inet_drv.c:12160`) never reads `desc->inet.active`, unlike
    // `TCP_REQ_RECV`'s `active || len != 8`: an active listener still queues accepts.
    assert_eq!(
        rig.control(&op(ACCEPT), &ZERO_TIMEOUT),
        async_ok_reply(0),
        "an active-mode listener still queues an accept"
    );
    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        vec![timed_out(0)],
        "the active-mode accept completes with `{{error, timeout}}`"
    );

    // Back to passive: the accept path is unaffected either way.
    let mut record = vec![INET_LOPT_ACTIVE];
    record.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        rig.control(&op(SETOPTS), &record),
        ctl_ok(),
        "`{{active,false}}` is set on the listener"
    );
    assert_eq!(
        rig.control(&op(GETOPTS), &[INET_LOPT_ACTIVE]),
        {
            let mut reply = ctl_ok();
            reply.push(INET_LOPT_ACTIVE);
            reply.extend_from_slice(&0u32.to_be_bytes());
            reply
        },
        "`getopts` reports the cleared `active` value"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &ZERO_TIMEOUT),
        async_ok_reply(1),
        "a passive listener queues an accept as well"
    );
    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        vec![timed_out(1)],
        "the passive accept completes with `{{error, timeout}}`"
    );
}

// A port holding **both** a pending accept and a pending recv is unreachable at this floor and is
// therefore not fabricated here: the accept guard requires `InetState::Listening` and the receive
// guard requires `InetState::Connected`, and one descriptor is exactly one of the two. The mixed
// drain (a close answering both a queued accept and a queued recv) is therefore pinned by the
// sibling real-socket suites, which can put a connected descriptor into a state that still has
// operations of both kinds queued.
