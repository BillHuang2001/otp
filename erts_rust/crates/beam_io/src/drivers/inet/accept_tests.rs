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
//! out, the descriptor-state bits `INET_REQ_GETSTATUS`(16) reports for the queued operations, and
//! the teardown drain — all through the crate's own [`PortRegistry`](crate::PortRegistry) /
//! [`Poller`](crate::Poller) path (the [`Rig`] the numeric-opcode suite uses), because an accept's
//! reply is only the **id**: the outcome is a
//! [`CompletionEvent::InetAsync`](crate::completion::CompletionEvent::InetAsync) staged on a
//! **later** port-task round, which is exactly what a live runtime drains.
//!
//! Mostly **hermetic**: no `erl` oracle, no connecting peer, no sleeps. Every listener binds
//! `127.0.0.1:0` (the kernel picks an ephemeral port, nothing is exposed) and most cases never
//! connect, which is what makes the queue's outcomes deterministic — a long deadline never fires,
//! a zero deadline fires on the next round, and a close drains what is left.
//!
//! Two families are **not** hermetic, and say so where they are defined: the readiness pair
//! ([`two_successive_connections_are_both_accepted`] and
//! [`the_accept_readiness_is_armed_on_enqueue_and_disarmed_when_the_list_drains`]) drives a real
//! loopback peer through a real [`Poller`](crate::Poller) round, because only a real connection can
//! prove the listener's read registration is re-armed on enqueue and dropped when the list drains;
//! and [`the_accept_replies_and_getstatus_rows_agree_with_the_oracle`] compares against the system
//! `erl`, skipping **loudly** when it is absent.
use super::op_queue::async_ok_reply;
use super::opcode_tests::{
    assert_agrees, erl_stdout, loopback_arg, op, open_arg, oracle_expression, oracle_replies, Call,
    Rig, Rule,
};
use super::opt::INET_INFINITY;
use super::wire::{ctl_ok, ctl_xerror, getstatus_reply};
use super::{
    make_tcp_inet, make_udp_inet, InetState, TcpInetDriver, INET_AF_INET, INET_LOPT_ACTIVE,
    INET_PROTO_DEFAULT, INET_REP_OK, INET_TYPE_DGRAM,
};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason};
use beam_common::BeamError;
use std::net::TcpStream;
use std::time::{Duration, Instant};

// The `erlang:port_control/3` codes this suite drives, under the C's `tcp_inet_ctl`/`inet_ctl`
// names (`inet_drv.c`): `OPEN`=1, `CLOSE`=2, `BIND`=6, `SETOPTS`=7, `GETOPTS`=8, `GETSTATUS`=16,
// `ACCEPT`=26, `LISTEN`=27. The arms live in `super`'s `control_opcode` and `super::udp`.
const OPEN: u32 = 1;
const CLOSE: u32 = 2;
const BIND: u32 = 6;
const SETOPTS: u32 = 7;
const GETOPTS: u32 = 8;
const GETSTATUS: u32 = 16;
const ACCEPT: u32 = 26;
const LISTEN: u32 = 27;

/// A listener with no accept queued: `INET_STATE_LISTENING` = `OPEN | INET_F_LISTEN` = `1 | 8`.
const LISTENING_STATUS: u32 = 9;

/// One queued accept: `9 | INET_F_ACC` (`0x20`) = `0x29`. Measured on the oracle.
const ONE_ACCEPT_STATUS: u32 = 41;

/// Two or more queued accepts: `9 | INET_F_ACC | INET_F_MULTI_CLIENT` (`0x100`) = `0x129`.
/// Measured on the oracle.
const MULTI_ACCEPT_STATUS: u32 = 297;

/// A 60 000 ms accept deadline: no round can reach it, so the queue stays put.
const LONG_TIMEOUT: [u8; 4] = [0, 0, 0xea, 0x60];

/// A zero deadline — the C's `driver_set_timer(port, 0)`, due at the next round.
const ZERO_TIMEOUT: [u8; 4] = [0, 0, 0, 0];

/// Asserts a `bind` reply is the C's `[1] ++ port_hi ++ port_lo` and returns the bound port.
fn bound_port(reply: &[u8]) -> u16 {
    assert_eq!(reply.len(), 3, "`bind` answers `[1, port_hi, port_lo]`, got {reply:?}");
    assert_eq!(reply[0], INET_REP_OK, "`bind`'s result tag is `INET_REP_OK`, got {reply:?}");
    u16::from_be_bytes([reply[1], reply[2]])
}

/// Opens, binds `127.0.0.1:0` (the kernel assigns the port) and listens, returning the bound port.
fn listening(rig: &mut Rig) -> u16 {
    assert_eq!(rig.control(&op(OPEN), &open_arg()), ctl_ok(), "`open` answers `[1]`");
    let port = bound_port(&rig.control(&op(BIND), &loopback_arg(0)));
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    assert_eq!(rig.control(&op(LISTEN), &[0, 5]), ctl_ok(), "`listen` answers `[1]`");
    port
}

/// Drains the completions the last round staged, without their port id: every case here has one
/// port, so the id would be the same in every element.
fn events(rig: &mut Rig) -> Vec<CompletionEvent> {
    rig.completions().into_iter().map(|done| done.event).collect()
}

/// The `{inet_async, Port, Id, {error, timeout}}` completion.
fn timed_out(id: u16) -> CompletionEvent {
    let result = InetAsyncResult::Error(InetReason::Timeout);
    CompletionEvent::InetAsync { id, result }
}

/// The `{inet_async, Port, Id, {error, closed}}` completion a teardown drains an operation with.
fn closed(id: u16) -> CompletionEvent {
    let result = InetAsyncResult::Error(InetReason::Closed);
    CompletionEvent::InetAsync { id, result }
}

/// The `getopts` reply a stored `{active, N}` reports: `[1] ++ [code, val_be32]`.
fn active_reply(value: u32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.push(INET_LOPT_ACTIVE);
    reply.extend_from_slice(&value.to_be_bytes());
    reply
}

#[test]
fn accept_on_a_non_listener_answers_einval_in_every_state() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));

    // Never opened: the accept guard is the C's combined "not listening **or** bad operand" test
    // (`inet_drv.c:12160`) and answers `einval` — never `exbadport` — even for a well-formed
    // 4-byte operand (measured on the oracle in every non-listening state).
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on a never-opened port answers `einval`"
    );
    assert_eq!(rig.control(&op(OPEN), &open_arg()), ctl_ok(), "`open` answers `[1]`");
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on an opened, non-listening port answers `einval`"
    );
    // Bound but not listening: `bind` does not move the descriptor out of `INET_STATE_OPEN`.
    assert_ne!(bound_port(&rig.control(&op(BIND), &loopback_arg(0))), 0, "bound port");
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("einval"),
        "accept on a bound, non-listening port answers `einval`"
    );
    assert_eq!(rig.control(&op(CLOSE), &[]), ctl_ok(), "`close` answers `[1]`");
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
    // length last (`inet_drv.c:12042`), so a socket-less port reports `exbadport` even for a
    // well-formed operand.
    assert_eq!(
        rig.control(&op(LISTEN), &[0, 5]),
        ctl_xerror("exbadport"),
        "`listen` before `open` answers `exbadport` (the state guard comes first)"
    );
    assert_eq!(rig.control(&op(OPEN), &open_arg()), ctl_ok(), "`open` answers `[1]`");
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
    assert_eq!(rig.control(&op(LISTEN), &[0, 5]), ctl_ok(), "a 2-byte backlog is accepted");

    // The accept operand is `Timeout(4)` and nothing else: 0, 3 and 5 bytes are all `len != 4`, so
    // they answer the same `einval` the non-listening guard gives — the C folds both faults into
    // one `if`, making them indistinguishable to the caller (measured).
    for operand in [&[][..], &[0, 0, 0][..], &[0, 0, 0, 0, 0][..]] {
        let len = operand.len();
        assert_eq!(
            rig.control(&op(ACCEPT), operand),
            ctl_xerror("einval"),
            "accept with a {len}-byte operand on a listener answers `einval`"
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

    // `INET_REQ_ACCEPT` is a case of `tcp_inet_ctl` only, so the number belongs to neither switch
    // for the datagram driver and the C answers `[0]exbadport` (measured in every datagram state).
    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        ctl_xerror("exbadport"),
        "accept on a socket-less udp_inet port answers `exbadport`"
    );
    // The datagram `open` operand names `SOCK_DGRAM` (the stream shape is refused with `einval`),
    // and an open datagram port is no more acceptable.
    let dgram = [INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT];
    assert_eq!(rig.control(&op(OPEN), &dgram), ctl_ok(), "datagram `open` answers `[1]`");
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
    assert!(rig.next_timeout().is_some(), "the zero deadline arms the port's `TIMEOUT` task");
    rig.round().expect("the round runs");
    assert_eq!(
        events(&mut rig),
        vec![timed_out(0)],
        "a zero deadline completes exactly once with `{{error, timeout}}`"
    );
    assert_eq!(rig.next_timeout(), None, "the completed operation leaves no deadline behind");
    // The list drained, so the descriptor-state bits go back to a bare listener
    // (`tcp_inet_multi_timeout`, `inet_drv.c:12607`-`:12611`).
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(LISTENING_STATUS),
        "a drained accept list reports `INET_STATE_LISTENING` again"
    );
    rig.round().expect("the round runs");
    assert_eq!(events(&mut rig), Vec::new(), "a second round reports nothing");

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

    // `INET_REQ_ACCEPT` uses the C's `enq_multi_op` list, which never refuses
    // (`inet_drv.c:12194`): repeats answer their own id — never `ealready` — in enqueue order.
    assert_eq!(rig.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(0), "first accept id 0");
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(ONE_ACCEPT_STATUS),
        "one queued accept reports `INET_STATE_ACCEPTING` (`9 | 0x20`)"
    );
    assert_eq!(rig.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(1), "second accept id 1");
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(MULTI_ACCEPT_STATUS),
        "two queued accepts add `INET_F_MULTI_CLIENT` (`9 | 0x120`)"
    );
    assert_eq!(rig.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(2), "third accept id 2");
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(MULTI_ACCEPT_STATUS),
        "three queued accepts report the same multi-accept bits as two"
    );

    // Nothing connected and no due deadline: a round completes nothing.
    rig.round().expect("the round runs");
    assert_eq!(events(&mut rig), Vec::new(), "nothing is completable yet");

    // The teardown drains every pending accept as `{error, closed}`, in enqueue order, exactly
    // once each (the C's `async_error_am_all(am_closed)`).
    rig.close();
    assert_eq!(
        events(&mut rig),
        vec![closed(0), closed(1), closed(2)],
        "`close` drains the three queued accepts in enqueue order"
    );
    rig.round().expect("the round runs");
    assert_eq!(events(&mut rig), Vec::new(), "a round after the drain reports nothing");
    assert!(!rig.contains(), "the closed port left the registry");
}

#[test]
fn closing_with_pending_accepts_drains_each_exactly_once() {
    let mut one = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut one);
    assert_eq!(one.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(0), "one accept queues");
    one.close();
    assert_eq!(events(&mut one), vec![closed(0)], "one pending accept is drained exactly once");
    assert_eq!(events(&mut one), Vec::new(), "the drain is exactly once: a second is empty");
    assert!(!one.contains(), "the closed port left the registry");

    let mut two = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut two);
    assert_eq!(two.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(0), "first accept queues");
    assert_eq!(two.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(1), "second accept queues");
    two.close();
    assert_eq!(
        events(&mut two),
        vec![closed(0), closed(1)],
        "two pending accepts are drained in enqueue order"
    );
    assert_eq!(events(&mut two), Vec::new(), "the drain is exactly once: a second is empty");
    assert!(!two.contains(), "the closed port left the registry");
}

#[test]
fn active_does_not_gate_the_accept() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    // `{active, true}` — the five-byte `[opt, val_be32]` record `INET_REQ_SETOPTS` walks.
    let record = |value: u32| {
        let mut record = vec![INET_LOPT_ACTIVE];
        record.extend_from_slice(&value.to_be_bytes());
        record
    };
    assert_eq!(rig.control(&op(SETOPTS), &record(1)), ctl_ok(), "`{{active,true}}` is set");
    assert_eq!(
        rig.control(&op(GETOPTS), &[INET_LOPT_ACTIVE]),
        active_reply(1),
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
    assert_eq!(rig.control(&op(SETOPTS), &record(0)), ctl_ok(), "`{{active,false}}` is set");
    assert_eq!(
        rig.control(&op(GETOPTS), &[INET_LOPT_ACTIVE]),
        active_reply(0),
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
// drain is pinned by the sibling real-socket suites, which can queue operations of both kinds on a
// connected descriptor that is then torn down.

/// The wall-clock cap on a loopback connect turning into a completion. A kernel-local connect
/// completes in microseconds; the cap only stops a genuinely broken round from hanging the suite.
const ACCEPT_WAIT: Duration = Duration::from_secs(10);

/// One **readiness** round: wait for the poller to report, then let the registry schedule *and*
/// run the tasks those events name — the loop `beam_rt` runs. Unlike [`Rig::round`] (which calls
/// `execute_port_tasks` directly, i.e. the `drive_async` poll point) the completion here can only
/// come from a real readiness event.
fn poll_round(rig: &mut Rig) {
    let events = match rig.poller.poll(Some(Duration::from_millis(100))) {
        Ok(events) => events,
        Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => return,
        Err(err) => panic!("the poll failed: {err:?}"),
    };
    rig.registry
        .handle_events(&mut rig.poller, &events)
        .expect("the ready events are dispatched");
}

/// Drives readiness rounds until one completion is staged, returning it — a **polled condition**
/// under [`ACCEPT_WAIT`], never a sleep-then-assert.
fn drive_until_completion(rig: &mut Rig, label: &str) -> Vec<CompletionEvent> {
    let deadline = Instant::now() + ACCEPT_WAIT;
    loop {
        poll_round(rig);
        let staged = events(rig);
        if !staged.is_empty() {
            return staged;
        }
        assert!(Instant::now() < deadline, "{label}: no completion within {ACCEPT_WAIT:?}");
    }
}

/// Whether the port's driver currently holds a **read** registration (the `PortArms` mask the
/// reactor gates an `Input` task on).
fn armed(rig: &Rig) -> bool {
    rig.registry.get(rig.id).expect("the port is live").read_armed()
}

/// Drives readiness until the accept queued as `id` completes, then **adopts** the descriptor it
/// hands over through [`TcpInetDriver::from_socket`] — the safe consumer of an
/// [`InetAsyncResult::Accepted`] (`from_socket` probes the kernel's state and the driver then owns
/// and closes the number). Asserts the classification so an adopted socket can never be silently
/// misreported.
fn accepted(rig: &mut Rig, id: u16, label: &str) -> TcpInetDriver {
    let staged = drive_until_completion(rig, label);
    assert_eq!(staged.len(), 1, "{label}: exactly one completion, got {staged:?}");
    match &staged[0] {
        CompletionEvent::InetAsync { id: got, result: InetAsyncResult::Accepted { fd } } => {
            assert_eq!(*got, id, "{label}: the completion carries the accept's own id");
            let driver = TcpInetDriver::from_socket(*fd)
                .expect("the accepted descriptor is adopted and classified");
            assert_eq!(
                driver.socket(),
                Some(*fd),
                "{label}: the driver owns the accepted descriptor"
            );
            assert_eq!(
                driver.state(),
                InetState::Connected,
                "{label}: an accepted socket is a connected one"
            );
            driver
        }
        other => panic!("{label}: expected an `Accepted` completion, got {other:?}"),
    }
}

/// Two connections in a row are each accepted, which is the observable of the C's re-arm: after a
/// completion the accept list empties, the listener is deregistered, and queueing the next accept
/// must register it again — otherwise the second peer would never be reported.
#[test]
fn two_successive_connections_are_both_accepted() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let port = listening(&mut rig);

    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(0),
        "the first accept queues with id 0"
    );
    let _client1 = TcpStream::connect(("127.0.0.1", port)).expect("a loopback peer connects");
    let first = accepted(&mut rig, 0, "the first accepted connection");

    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(1),
        "a second accept queues with the next id"
    );
    let _client2 =
        TcpStream::connect(("127.0.0.1", port)).expect("a second loopback peer connects");
    let second = accepted(&mut rig, 1, "the second accepted connection");

    assert_eq!(events(&mut rig), Vec::new(), "each accept is reported exactly once");
    assert!(rig.contains(), "the listener is still live after accepting twice");
    assert_ne!(
        first.socket(),
        second.socket(),
        "the two connections are two distinct descriptors"
    );
}

/// The listener's **read registration** follows the accept list: armed while accepts are queued,
/// disarmed when the list drains (the C's `sock_select(FD_ACCEPT, 0)`), and re-armed by an enqueue
/// even when the connection it must serve arrived while nothing was queued.
#[test]
fn the_accept_readiness_is_armed_on_enqueue_and_disarmed_when_the_list_drains() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let port = listening(&mut rig);

    assert!(armed(&rig), "a listening port is read-registered from `listen` on");

    assert_eq!(rig.control(&op(ACCEPT), &LONG_TIMEOUT), async_ok_reply(0), "the accept queues");
    assert!(armed(&rig), "queueing an accept leaves the listener read-armed");

    let _client1 = TcpStream::connect(("127.0.0.1", port)).expect("a loopback peer connects");
    accepted(&mut rig, 0, "the first accepted connection");
    assert!(
        !armed(&rig),
        "a drained accept list deregisters the listener (`disarm_accepts`)"
    );

    // A connection that arrives while **nothing** is queued. Without the re-arm on enqueue its
    // readiness would never be reported again, so this is the state the re-arm exists for.
    let _client2 =
        TcpStream::connect(("127.0.0.1", port)).expect("a second loopback peer connects");
    assert!(!armed(&rig), "an arrived connection does not arm the listener by itself");

    assert_eq!(
        rig.control(&op(ACCEPT), &LONG_TIMEOUT),
        async_ok_reply(1),
        "the second accept queues against the disarmed listener"
    );
    assert!(armed(&rig), "an enqueue re-arms the listener");

    // Only poll + `handle_events` from here: the completion is a readiness event on the already
    // arrived connection, never the `drive_async` poll point.
    let second = accepted(&mut rig, 1, "the already-arrived connection");
    assert!(!armed(&rig), "the drained list deregisters the listener again");

    // With the list empty and both peers live, the listener reports nothing at all.
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        poll_round(&mut rig);
        assert_eq!(events(&mut rig), Vec::new(), "an empty accept list produces no completion");
    }
    assert!(rig.contains(), "the listener is still live");
    assert_eq!(second.state(), InetState::Connected, "the second connection is connected");
}

// ---------------------------------------------------------------------------
// The accept list's timer: the earliest deadline expires the list's **head**
// ---------------------------------------------------------------------------
//
// The port keeps **one** timer, and the C stores one `MultiTimerData` per *finite* accept deadline
// in a list sorted by `when` (`add_multi_timer`, `inet_drv.c:15744`-`:15790`), arming the port's
// timer only when the new entry became the head (`if (!s) driver_set_timer(port, timeout)`). Its
// expiry runs `fire_multi_timers` (`:15650`) → `tcp_inet_multi_timeout` (`:12600`), which removes
// the **first** entry of the accept list (`remove_multi_op`, `:2379`-`:2405` — the head, since
// every accept here comes from the one `driver_caller`) and answers *that* id with
// `{error, timeout}`; it then continues while the earliest remaining deadline is already due and
// otherwise re-arms from the survivors. An `INET_INFINITY` accept adds **no** entry at all
// (`mtd` stays `NULL` at the enqueue sites, `:12198`-`:12210`), so it can neither arm the timer nor
// be the operation a firing reports.
//
// Measured on the oracle (OTP 29.1.1 / erts-17.1): `gen_tcp:listen(0,[binary,{active,false},
// {reuseaddr,true},{backlog,10}])`, one `erlang:port_control(L, 26 + 16#03f1a300, <<Timeout:32>>)`
// per queued accept, every `{inet_async,_,_,_}` message collected for 4000 ms — `[infinity, 50]`
// ⇒ `[{inet_async,L,0,{error,timeout}}]`; `[infinity, 50, 60]` ⇒ `[0 timeout, 1 timeout]`;
// `[50, 500]` ⇒ `[0 timeout, 1 timeout]`.

/// The C's `INET_INFINITY` (`0xffffffff`) accept operand: the operation joins the accept list but
/// contributes **no** timer.
const INFINITY: [u8; 4] = INET_INFINITY.to_be_bytes();

/// The `Timeout(4)` operand of `INET_REQ_ACCEPT`(26) for a finite deadline in milliseconds.
fn timeout_operand(timeout_ms: u32) -> [u8; 4] {
    timeout_ms.to_be_bytes()
}

/// One port-task round, returning the completions it staged.
fn round_events(rig: &mut Rig, label: &str) -> Vec<CompletionEvent> {
    rig.round()
        .unwrap_or_else(|err| panic!("{label}: the round failed: {err:?}"));
    events(rig)
}

/// Runs port-task rounds until one stages a completion, returning what it staged.
///
/// The port's `TIMEOUT` task only becomes runnable once the driver's **earliest** deadline has
/// passed ([`crate::PortTaskQueue`](crate::port_task::PortTaskQueue)'s `pop_next` leaves a
/// not-yet-due `TIMEOUT` queued), so the wait polls the **condition** under [`ACCEPT_WAIT`] — never
/// a wall-clock budget tuned for an idle host.
fn drive_until_timer(rig: &mut Rig, label: &str) -> Vec<CompletionEvent> {
    let guard = Instant::now() + ACCEPT_WAIT;
    loop {
        let staged = round_events(rig, label);
        if !staged.is_empty() {
            return staged;
        }
        assert!(
            Instant::now() < guard,
            "{label}: no timer completion within {ACCEPT_WAIT:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The mixed queue's deadline: an **infinite** head and a 50 ms tail. The tail's expiry reports the
/// **head's** id — the only completion the oracle produces — and leaves the infinite accept queued
/// with no timer.
#[test]
fn the_earliest_accept_deadline_reports_the_head_not_the_tail() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    assert_eq!(
        rig.control(&op(ACCEPT), &INFINITY),
        async_ok_reply(0),
        "the infinite accept answers id 0"
    );
    assert_eq!(rig.next_timeout(), None, "an `INET_INFINITY` accept arms no timer at all");
    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(50)),
        async_ok_reply(1),
        "the 50 ms accept answers id 1"
    );
    // The port's single timer comes from the **earliest** deadline in the list (`add_multi_timer`,
    // `inet_drv.c:15744`), so an infinite head cannot suppress the tail's deadline.
    assert!(
        rig.next_timeout().is_some(),
        "the tail's deadline arms the port's `TIMEOUT` task"
    );

    // Nothing is completable before the deadline: no peer connected and the timer is still ahead.
    assert_eq!(
        round_events(&mut rig, "before the deadline"),
        Vec::new(),
        "nothing completes before the 50 ms deadline"
    );

    // The oracle's one measured completion: the **head's** id — not the id whose deadline fired.
    assert_eq!(
        drive_until_timer(&mut rig, "the mixed queue's deadline"),
        vec![timed_out(0)],
        "the tail's deadline reports the list head (id 0), never the tail (id 1)"
    );

    // The infinite survivor is untouched: it declares no deadline, so nothing can fire again.
    assert_eq!(rig.next_timeout(), None, "the surviving accept declares no deadline");
    let quiet = Instant::now() + Duration::from_millis(100);
    while Instant::now() < quiet {
        assert_eq!(
            round_events(&mut rig, "after the head's timeout"),
            Vec::new(),
            "the infinite survivor never times out, and the timeout is reported exactly once"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // The C restores `INET_STATE_LISTENING` only when the whole list drains
    // (`tcp_inet_multi_timeout`, `inet_drv.c:12607`-`:12611`), so the oracle keeps reporting the
    // multi bit after this timeout (`[1,0,0,1,41]`); `AsyncQueue::accept_state_bits` derives the
    // bits from the surviving list instead and answers `[1,0,0,0,41]`. That divergence is recorded
    // in `CONTEXT.md` › *Known Issues* and pinned here so changing it is deliberate.
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(ONE_ACCEPT_STATUS),
        "the floor derives the accept bits from the surviving list (the oracle keeps the multi bit)"
    );
    assert!(rig.contains(), "the listener is still live");
}

/// Three queued accepts, two finite deadlines: each firing reports the list **head**, so the
/// infinite tail's presence is invisible — `[infinity, 50, 60]` reports ids 0 then 1, in list
/// order, and never id 2.
#[test]
fn two_due_deadlines_report_the_list_head_twice_in_order() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    assert_eq!(rig.control(&op(ACCEPT), &INFINITY), async_ok_reply(0), "infinite accept id 0");
    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(50)),
        async_ok_reply(1),
        "the 50 ms accept answers id 1"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(60)),
        async_ok_reply(2),
        "the 60 ms accept answers id 2"
    );
    assert!(rig.next_timeout().is_some(), "the 50 ms deadline arms the timer");

    // The first firing reports id 0 — the head — although the deadline that fired is id 1's.
    let mut staged = drive_until_timer(&mut rig, "the 50 ms deadline");
    assert_eq!(
        staged[0],
        timed_out(0),
        "the earliest deadline reports the list head, not the operation that owns it"
    );

    // The survivor keeps its own deadline, so a second firing reports the new head (id 1). The
    // two can land in one round when both deadlines are already past, hence the accumulation.
    let guard = Instant::now() + ACCEPT_WAIT;
    while staged.len() < 2 {
        assert!(
            Instant::now() < guard,
            "the 60 ms survivor never fired within {ACCEPT_WAIT:?}: {staged:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
        let more = round_events(&mut rig, "the 60 ms deadline");
        staged.extend(more);
    }
    assert_eq!(
        staged,
        vec![timed_out(0), timed_out(1)],
        "the two deadlines report the list head twice, in list order"
    );

    // Only the infinite id 2 is left: no deadline, so nothing more is ever reported.
    assert_eq!(rig.next_timeout(), None, "the infinite survivor leaves no deadline");
    for label in ["after both deadlines", "and again"] {
        assert_eq!(
            round_events(&mut rig, label),
            Vec::new(),
            "the infinite accept (id 2) is never reported by a timeout"
        );
    }
}

/// The reverse order of [`two_finite_deadlines_report_the_head_then_the_survivor`]: the **head**
/// owns the *later* deadline. The earliest entry fires first and reports the head; the head's own
/// entry is not removed by that firing, so it stays armed and later expires the survivor.
///
/// Measured on the oracle with operands `<<500:32>>, <<50:32>>`:
/// `{inet_async, L, 0, {error,timeout}}` at ~51 ms and `{inet_async, L, 1, {error,timeout}}` at
/// ~501 ms — two distinct firings, never one batch.
#[test]
fn a_heads_own_later_timer_still_expires_the_survivor() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(500)),
        async_ok_reply(0),
        "the 500 ms accept answers id 0"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(50)),
        async_ok_reply(1),
        "the 50 ms accept answers id 1"
    );

    assert_eq!(
        drive_until_timer(&mut rig, "the 50 ms deadline"),
        vec![timed_out(0)],
        "the earliest entry fires first and reports the head (id 0), not its own operation"
    );
    // `remove_multi_op` was called with `timeout_p == NULL`, so the head's own 500 ms entry is
    // still in `desc->mtd` and keeps the port's timer armed.
    assert!(
        rig.next_timeout().is_some(),
        "the head's own later timer entry survives the firing that removed it"
    );
    assert_eq!(
        round_events(&mut rig, "between the two firings"),
        Vec::new(),
        "the survivor is not reported until that entry expires"
    );
    assert_eq!(
        drive_until_timer(&mut rig, "the head's own 500 ms timer"),
        vec![timed_out(1)],
        "the head's own entry expires the survivor, with the survivor's id"
    );
    assert_eq!(rig.next_timeout(), None, "the drained list leaves no timer armed");
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(LISTENING_STATUS),
        "a fully drained accept list reports a bare listener again"
    );
}

/// Two finite deadlines: the head reports at its **own** deadline and the survivor at its own, one
/// completion per firing, and the drained list leaves a bare listener again.
#[test]
fn two_finite_deadlines_report_the_head_then_the_survivor() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    listening(&mut rig);

    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(50)),
        async_ok_reply(0),
        "the 50 ms accept answers id 0"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &timeout_operand(500)),
        async_ok_reply(1),
        "the 500 ms accept answers id 1"
    );

    assert_eq!(
        drive_until_timer(&mut rig, "the 50 ms deadline"),
        vec![timed_out(0)],
        "the 50 ms deadline reports the head and leaves the survivor queued"
    );
    // The survivor's own deadline is still 450 ms away, so it cannot be due in this round: the
    // second completion is a separate firing, never a batch (`fire_multi_timers` re-arms with
    // `next_timeout > 0`).
    assert!(
        rig.next_timeout().is_some(),
        "the survivor re-arms the port's timer from its own deadline"
    );
    assert_eq!(
        round_events(&mut rig, "between the two deadlines"),
        Vec::new(),
        "the survivor is not reported before its own deadline"
    );
    assert_eq!(
        drive_until_timer(&mut rig, "the 500 ms deadline"),
        vec![timed_out(1)],
        "the survivor reports at its own deadline, with its own id"
    );
    assert_eq!(rig.next_timeout(), None, "an empty accept list declares no deadline");

    // The whole list drained, which is exactly the case where the C restores
    // `INET_STATE_LISTENING` (`tcp_inet_multi_timeout`, `inet_drv.c:12607`-`:12611`).
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(LISTENING_STATUS),
        "a fully drained accept list reports a bare listener again"
    );
    assert_eq!(
        round_events(&mut rig, "after the list drained"),
        Vec::new(),
        "each accept timeout is reported exactly once"
    );
    assert!(rig.contains(), "the listener is still live");
}

/// An `INET_INFINITY` accept that outlives the head's timeout still takes the later connection —
/// the fix must report the head **without** dropping the survivor.
///
/// Measured rows: `[infinity, 50]` with a connect at 120 ms and `[50, infinity]` with one at 200 ms
/// both answer `[0 timeout, 1 {ok, Port}]`.
#[test]
fn an_infinite_accept_survives_the_heads_timeout_and_takes_the_later_connection() {
    // Row 4: the infinite op is the **head**, the finite one is the tail.
    let mut mixed = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let port = listening(&mut mixed);
    assert_eq!(
        mixed.control(&op(ACCEPT), &INFINITY),
        async_ok_reply(0),
        "the infinite accept answers id 0"
    );
    assert_eq!(
        mixed.control(&op(ACCEPT), &timeout_operand(50)),
        async_ok_reply(1),
        "the finite accept answers id 1"
    );
    assert_eq!(
        drive_until_timer(&mut mixed, "the mixed queue's deadline"),
        vec![timed_out(0)],
        "the tail's deadline reports the head (id 0)"
    );
    // The surviving op keeps its place in the list but its timer entry was consumed by that firing
    // (`remove_multi_op` with `timeout_p == NULL`), so the port declares no deadline at all —
    // measured: `[infinity, 50]` answers exactly one `{error, timeout}` and nothing later.
    assert_eq!(
        mixed.next_timeout(),
        None,
        "the firing consumed the survivor's timer entry, so no deadline is armed"
    );
    let _client = TcpStream::connect(("127.0.0.1", port)).expect("a loopback peer connects");
    let from_mixed = accepted(&mut mixed, 1, "the surviving infinite accept");
    assert_eq!(
        from_mixed.state(),
        InetState::Connected,
        "the surviving accept takes the later connection"
    );
    assert_eq!(
        round_events(&mut mixed, "after the survivor was accepted"),
        Vec::new(),
        "the accepted operation is reported exactly once"
    );
    assert!(mixed.contains(), "the listener is still live after serving both");

    // Row 2, the mirror image: the finite op is the head and the infinite one the tail.
    let mut mirrored = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let port = listening(&mut mirrored);
    assert_eq!(
        mirrored.control(&op(ACCEPT), &timeout_operand(50)),
        async_ok_reply(0),
        "the finite accept answers id 0"
    );
    assert_eq!(
        mirrored.control(&op(ACCEPT), &INFINITY),
        async_ok_reply(1),
        "the infinite accept answers id 1"
    );
    assert_eq!(
        drive_until_timer(&mut mirrored, "the head's deadline"),
        vec![timed_out(0)],
        "the head reports at its own deadline"
    );

    let _client = TcpStream::connect(("127.0.0.1", port)).expect("a loopback peer connects");
    let from_mirror = accepted(&mut mirrored, 1, "the infinite tail");
    assert_eq!(
        from_mirror.state(),
        InetState::Connected,
        "the infinite tail survives the head's timeout and is accepted later"
    );
    assert!(mirrored.contains(), "the listener is still live after the mirror row");
}

/// The `erl`-oracle matrix for the accept path: the reply ids, the state bits `getstatus` reports
/// for the queue, and the two bare-port refusals — byte-compared against the system
/// `tcp_inet`/`udp_inet` drivers (skips **loudly** without `erl`).
#[test]
fn the_accept_replies_and_getstatus_rows_agree_with_the_oracle() {
    // The measured sequence: two queued accepts, each visible in `getstatus`'s state bits —
    // `41` = listening (9) + `INET_F_ACC` (0x20), `297` = that + `INET_F_MULTI_CLIENT` (0x100).
    let calls = vec![
        Call::req(OPEN, open_arg(), Rule::Exact),
        Call::req(BIND, loopback_arg(0), Rule::BoundPort),
        Call::req(LISTEN, [0u8, 5], Rule::Exact),
        Call::req(ACCEPT, LONG_TIMEOUT, Rule::Exact),
        Call::req(GETSTATUS, Vec::<u8>::new(), Rule::Exact),
        Call::req(ACCEPT, LONG_TIMEOUT, Rule::Exact),
        Call::req(GETSTATUS, Vec::<u8>::new(), Rule::Exact),
    ];
    let labels = [
        "open",
        "bind",
        "listen",
        "accept #1",
        "getstatus with one queued accept",
        "accept #2",
        "getstatus with two queued accepts",
    ];

    let Some(dump) = erl_stdout(&oracle_expression("tcp_inet", &calls)) else {
        eprintln!("SKIP: no `erl` — the accept/getstatus oracle matrix was not compared");
        return;
    };
    eprintln!("the accept/getstatus oracle dump: {dump}");
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), calls.len(), "the oracle answered every call: {dump:?}");

    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let rust = rig.run(&calls);
    for index in 0..calls.len() {
        assert_agrees(labels[index], calls[index].rule, &oracle[index], &rust[index]);
    }

    // The same rows spelled out, so a silently loosened rule could not hide a drift.
    assert_eq!(rust[0], ctl_ok(), "`open` answers `[1]`");
    assert_eq!(rust[2], ctl_ok(), "`listen` answers `[1]`");
    assert_eq!(rust[3], async_ok_reply(0), "the first accept answers id 0");
    assert_eq!(
        rust[4],
        getstatus_reply(ONE_ACCEPT_STATUS),
        "one queued accept sets `INET_F_ACC`"
    );
    assert_eq!(rust[5], async_ok_reply(1), "the second accept answers id 1");
    assert_eq!(
        rust[6],
        getstatus_reply(MULTI_ACCEPT_STATUS),
        "two queued accepts add `INET_F_MULTI_CLIENT`"
    );

    // On a **bare** port the accept guard answers before any state exists: `einval` on the stream
    // driver (its combined non-listening/bad-operand test) and `exbadport` on the datagram one,
    // whose switch does not own the number at all.
    let bare = vec![Call::req(ACCEPT, LONG_TIMEOUT, Rule::Exact)];
    let Some(dump) = erl_stdout(&oracle_expression("tcp_inet", &bare)) else {
        eprintln!("SKIP: no `erl` — the bare-port accept matrix was not compared");
        return;
    };
    eprintln!("the bare `tcp_inet` accept oracle dump: {dump}");
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 1, "the oracle answered the bare accept: {dump:?}");
    let mut stream = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let rust = stream.run(&bare);
    assert_agrees("bare tcp_inet accept", Rule::Exact, &oracle[0], &rust[0]);
    assert_eq!(rust[0], ctl_xerror("einval"), "a bare `tcp_inet` accept answers `einval`");

    let Some(dump) = erl_stdout(&oracle_expression("udp_inet", &bare)) else {
        eprintln!("SKIP: no `erl` — the bare-port datagram accept matrix was not compared");
        return;
    };
    eprintln!("the bare `udp_inet` accept oracle dump: {dump}");
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 1, "the oracle answered the bare datagram accept: {dump:?}");
    let mut datagram = Rig::new(make_udp_inet().expect("the udp_inet driver"));
    let rust = datagram.run(&bare);
    assert_agrees("bare udp_inet accept", Rule::Exact, &oracle[0], &rust[0]);
    assert_eq!(
        rust[0],
        ctl_xerror("exbadport"),
        "the datagram driver's switch does not own the accept number"
    );
}
