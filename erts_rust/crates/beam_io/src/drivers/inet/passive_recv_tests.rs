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

//! The passive `gen_tcp:recv` **park**: what a `TCP_REQ_RECV`(42) does between the moment its
//! control reply hands out an id and the moment the socket can answer it.
//!
//! `gen_tcp:recv/2,3` is `prim_inet:recv0/3` (`erts/preloaded/src/prim_inet.erl:801`-`:810`), which
//! calls `prim_inet:async_recv/3` (`:820`) and then **blocks in a `receive`** on
//! `{inet_async, S, Ref, Status}`. Everything about the user-visible `recv` therefore hangs off the
//! driver's park: the control reply must return immediately (it is the id, never the data), the
//! operation must survive an idle socket, and the completion must arrive **once** — from the
//! readiness edge, from a deadline, or from a teardown.
//!
//! The C's shape, arm by arm (`erts/emulator/drivers/common/inet_drv.c`):
//!
//! * `tcp_inet_ctl`'s `TCP_REQ_RECV` arm (`:12265`-`:12325`) takes the id (`NEW_ASYNC_ID`,
//!   `:1549`), enqueues the operation (`enq_async`, `:12307`), arms read readiness with
//!   `sock_select(INETP(desc), (FD_READ|FD_CLOSE), 1)` (`:12315`), arms the port timer with
//!   `driver_set_timer` when the operand is not `INET_INFINITY`, and answers `ctl_reply_ok` +
//!   `<<Id:16/big>>` (`:12319`) — **before** any byte has been read;
//! * the read side then really does nothing when the socket is empty: `tcp_recv` (`:13130`) leaves
//!   the operation enqueued on `EWOULDBLOCK` and only the next readiness callback retries it;
//! * a deadline expires through the port timer (`tcp_inet_recv_timeout`, `:12480`), which attempts
//!   the receive once more and otherwise answers `{error, timeout}`;
//! * a teardown answers every pending operation with `{error, closed}`
//!   (`async_error_am_all(am_closed)`, `:2597`-`:2608`, reached from `tcp_close_check` `:11887`,
//!   `tcp_recv_closed` `:12829` and `tcp_recv_error` `:12877`).
//!
//! Two consequences are the whole point of this suite and are asserted here rather than assumed:
//!
//! 1. **A would-block is a park, not an error.** Nothing is delivered on the control reply, nothing
//!    lands on the port's passive `{Port, {data, …}}` channel (the driver's input queue stays
//!    empty), and no completion exists until the socket has something to say — however many
//!    port-task rounds run in between.
//! 2. **The completion is produced at a later round, never inside the control call**, and exactly
//!    once. Both of the crate's drive points are pinned: the reactor's readiness edge
//!    (`Driver::ready_input` → `async_ready_input`) and the round's own poll
//!    (`Driver::drive_async` → `drive_async_ops`, run once per round by
//!    [`PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks)).
//!
//! Every wait is a **condition** under a hang guard, never a wall-clock budget tuned for an idle
//! host: a loop rounds, drains and re-rounds until the condition holds or the guard expires.
//!
//! The sibling [`async_tests`](super::async_tests) suite pins the *shapes* of the same completion
//! channel (the `{header, N}` split, the framed payloads, "exactly `n` bytes", the caller tag);
//! this one pins the **park's** timing and its `erl`-measured row-for-row agreement.

use std::io::Write;
use std::time::{Duration, Instant};

use super::super::accept_tests::poll_round;
use super::super::async_tests::{async_events, await_events, linked, recv_operand};
use super::super::opcode_tests::{escript_stdout, op, Rig};
use super::super::opt::INET_INFINITY;
use super::super::wire::{ctl_xerror, INET_REP_ERROR, INET_REP_OK};
use crate::completion::{InetAsyncResult, InetReason};
use crate::port_task::ReadMessage;

/// `TCP_REQ_RECV` (`inet_drv.c:110`): the asynchronous receive.
const TCP_REQ_RECV: u32 = 42;
/// `INET_REQ_GETSTATUS` (`inet_drv.c:612`): the descriptor-state read-back. It is how both sides
/// prove the port was really **connected** when the park began, and really **closed** after the
/// peer's close.
const INET_REQ_GETSTATUS: u32 = 16;
/// `INET_STATE_CONNECTED` (`inet_drv.c:668`).
const INET_STATE_CONNECTED: u32 = 5;
/// `INET_STATE_CLOSED` (`inet_drv.c:664`).
const INET_STATE_CLOSED: u32 = 0;

/// The upper bound a condition wait may take before the test fails: a hang guard, never a
/// wall-clock budget tuned for an idle host.
const HANG_GUARD: Duration = Duration::from_secs(10);
/// How long one condition-wait iteration sleeps between rounds.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How long a park is watched before "nothing arrived" is believed. The oracle rows below are all
/// bound to this same window, so a floor that answered early would disagree with the `erl` probe
/// and not merely with a sleep.
const PARK_WINDOW: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Driving the park
// ---------------------------------------------------------------------------

/// Drives port-task rounds for `window` and answers every completion the port staged, without
/// sleeping a fixed budget between them: the loop rounds, drains, sleeps one [`POLL_INTERVAL`] and
/// repeats. A park is exactly "this came back empty".
fn park_window(rig: &mut Rig, window: Duration) -> Vec<(u16, InetAsyncResult)> {
    let deadline = Instant::now() + window;
    let mut events = Vec::new();
    loop {
        rig.round().expect("a port-task round");
        events.extend(async_events(&rig.completions()));
        if Instant::now() >= deadline {
            return events;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Drives **readiness** rounds (a real `poll(2)` plus the dispatch of what it reports) until one
/// completion is staged, returning it — a polled condition under [`HANG_GUARD`].
///
/// This is the reactor's route: `beam_rt` owns the poll loop, so a completion that only ever
/// appeared from a readiness event would still be a *correct* floor here — which is why the two
/// routes are pinned separately.
fn await_events_on_the_edge(rig: &mut Rig, want: usize) -> Vec<(u16, InetAsyncResult)> {
    let guard = Instant::now() + HANG_GUARD;
    let mut events = Vec::new();
    while events.len() < want {
        poll_round(rig);
        events.extend(async_events(&rig.completions()));
        assert!(
            Instant::now() < guard,
            "only {} of {want} completions arrived within {HANG_GUARD:?}: {events:?}",
            events.len()
        );
    }
    events
}

/// The `TCP_REQ_RECV` reply for a well-formed operand, asserting it is the `[1] ++ <<Id:16/big>>`
/// of `inet_drv.c:12319` — the only thing the control call itself may return.
fn arm_receive(rig: &mut Rig, timeout_ms: i32) -> u16 {
    let reply = rig.control(&op(TCP_REQ_RECV), &recv_operand(timeout_ms, 0));
    assert_eq!(
        reply.len(),
        3,
        "the control reply is `[1] ++ <<Id:16/big>>`, never the data: {reply:?}"
    );
    assert_eq!(reply[0], INET_REP_OK, "the result tag");
    u16::from_be_bytes([reply[1], reply[2]])
}

/// The `INET_REQ_GETSTATUS` state code — `5` connected, `0` closed.
fn state_code(rig: &mut Rig) -> u8 {
    let reply = rig.control(&op(INET_REQ_GETSTATUS), &[]);
    assert_eq!(reply.len(), 5, "`[1] ++ be32(state)`: {reply:?}");
    reply[4]
}

/// Whether the port's driver currently holds a **read** registration — the crate's mirror of the
/// C's `sock_select(…, (FD_READ|FD_CLOSE), 1)` at the `TCP_REQ_RECV` site (`inet_drv.c:12315`).
fn read_armed(rig: &Rig) -> bool {
    rig.registry
        .get(rig.id)
        .expect("the port is live")
        .read_armed()
}

// ---------------------------------------------------------------------------
// Row 1 — the park itself
// ---------------------------------------------------------------------------

/// **A passive receive armed before any peer byte parks.** The control call answers the id (never
/// the data), the read side is selected, and then *nothing* happens: no completion and no bytes on
/// the port's passive `{Port, {data, …}}` channel, however many rounds run. The peer's write ends
/// the park with exactly one `{inet_async, Port, Id, {ok, Data}}`, and a second drain is empty.
///
/// The C's sequence is `enq_async` → `sock_select(FD_READ|FD_CLOSE, 1)` → `ctl_reply_ok + id`
/// (`inet_drv.c:12307`-`:12319`), then `tcp_recv` leaving the operation enqueued on
/// `EWOULDBLOCK` (`:13130`).
#[test]
fn the_passive_receive_parks_before_a_peer_byte_and_completes_on_the_edge() {
    let (mut port, mut peer) = linked();
    assert_eq!(
        state_code(&mut port),
        INET_STATE_CONNECTED as u8,
        "the park starts on a connected socket"
    );

    assert_eq!(arm_receive(&mut port, INET_INFINITY), 0, "the first id is 0");
    assert!(
        read_armed(&port),
        "the enqueue selects the read side (`sock_select(FD_READ|FD_CLOSE, 1)`, `:12315`)"
    );

    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "an empty socket parks: no completion may be invented while there is nothing to read"
    );
    assert_eq!(
        port.input_len(),
        0,
        "and nothing may reach the port's passive `{{Port,{{data,…}}}}` channel"
    );
    assert!(port.next_timeout().is_none(), "an infinite deadline arms no timer");

    // The peer's byte ends the park — through the reactor's readiness edge, the route `beam_rt`
    // really runs.
    peer.write_all(b"abc").expect("the peer writes");
    let events = await_events_on_the_edge(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"abc".to_vec()))
        )],
        "the readiness edge completes the parked operation with its own id"
    );
    assert_eq!(
        port.input_len(),
        0,
        "a completed receive never also reports through the passive channel"
    );
    assert!(
        port.completions().is_empty(),
        "a completion is drained exactly once"
    );
}

/// The same park, completed by the **round's** drive point instead of a readiness edge: the
/// registry runs `Driver::drive_async` once per port-task round
/// ([`PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks)), and that call
/// retries the pending read. A parked operation must therefore also complete when the poller
/// reports nothing at all — the property that keeps a `recv` correct behind a scheduler that
/// happens to run the port without an edge.
#[test]
fn the_park_is_completed_by_the_rounds_async_drive_alone() {
    let (mut port, mut peer) = linked();
    assert_eq!(arm_receive(&mut port, INET_INFINITY), 0);
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "parked before the peer writes"
    );

    peer.write_all(b"driven").expect("the peer writes");
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"driven".to_vec()))
        )],
        "the round's async drive completes the parked operation"
    );
    assert!(port.completions().is_empty(), "drained once");
}

// ---------------------------------------------------------------------------
// Row 2 — the single receive slot while it parks
// ---------------------------------------------------------------------------

/// **One receive slot** (`INET_MAX_ASYNC`, `inet_drv.c:1069`): a second `TCP_REQ_RECV` while the
/// first still parks is `[0]ealready` (`ctl_error(EALREADY)`, `:12307`), and — the C's
/// `NEW_ASYNC_ID` (`:1549`) — the refused call has **already** taken an id, so the next accepted
/// operation is id `+2` from the parked one. The parked operation is untouched by the refusal and
/// still completes with its own id.
#[test]
fn a_second_passive_receive_is_ealready_while_the_first_parks() {
    let (mut port, mut peer) = linked();
    assert_eq!(arm_receive(&mut port, INET_INFINITY), 0, "the parked receive");
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        ctl_xerror("ealready"),
        "the single receive slot refuses a second operation"
    );
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "the refusal changes nothing about the parked operation"
    );

    peer.write_all(b"only-one").expect("the peer writes");
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"only-one".to_vec()))
        )],
        "the parked operation still owns the completion"
    );

    assert_eq!(
        arm_receive(&mut port, INET_INFINITY),
        2,
        "the refused enqueue consumed id 1, so the next operation is id 2"
    );
}

// ---------------------------------------------------------------------------
// Row 3 — the deadline
// ---------------------------------------------------------------------------

/// **A zero-timeout receive parks and then fails with `{error, timeout}`**: the reply is still just
/// the id (`inet_drv.c:12319`), the deadline is declared to the port (`Driver::next_timeout`, the
/// C's `driver_set_timer`), and the outcome arrives on the completion channel at a later round —
/// once, from `tcp_inet_recv_timeout` (`:12480`). An exhausted deadline clears the port's timer.
#[test]
fn a_zero_timeout_parks_then_completes_with_a_timeout() {
    let (mut port, _peer) = linked();
    assert_eq!(arm_receive(&mut port, 0), 0, "a zero deadline still answers the id");
    assert!(
        port.completions().is_empty(),
        "and still reports nothing from the control call"
    );
    assert!(
        port.next_timeout().is_some(),
        "a pending operation declares its deadline to the port's single TIMEOUT task"
    );

    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Timeout))],
        "the timer answers the operation's own id"
    );
    assert!(
        port.next_timeout().is_none(),
        "an empty queue declares no deadline, so the port clears its TIMEOUT task"
    );
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "a timeout is reported exactly once"
    );
}

/// **A finite deadline expires as a timeout, and not before it.** The park is watched for less than
/// the operand's budget and must stay silent, which is the half a floor that answered early would
/// fail; then the timer fires with `{error, timeout}`, exactly once.
#[test]
fn a_finite_timeout_expires_as_a_timeout() {
    let (mut port, _peer) = linked();
    assert_eq!(arm_receive(&mut port, 250), 0, "the id is handed out at once");
    assert!(port.next_timeout().is_some(), "the deadline is declared");

    assert!(
        park_window(&mut port, Duration::from_millis(50)).is_empty(),
        "a 250 ms deadline has not expired after 50 ms: the park must still be silent"
    );

    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Timeout))],
        "the deadline answers the operation's own id"
    );
    assert!(port.next_timeout().is_none(), "the timer is cleared");
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "reported exactly once"
    );
}

/// **Bytes that have already arrived win over the deadline.** The C attempts the receive at
/// enqueue time, so `Timeout = 0` on a socket with buffered data delivers the data instead of
/// reporting `{error, timeout}` — `tcp_recv` runs before the port's timer can be observed
/// (`inet_drv.c:12319` then `:12325`).
#[test]
fn a_zero_deadline_delivers_data_that_already_arrived() {
    let (mut port, mut peer) = linked();
    peer.write_all(b"buffered").expect("the peer writes");

    assert_eq!(arm_receive(&mut port, 0), 0, "the reply is the id either way");
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"buffered".to_vec()))
        )],
        "the buffered bytes win over the expired deadline"
    );
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "and no timeout follows the delivery"
    );
}

// ---------------------------------------------------------------------------
// Row 4 — teardown while parked
// ---------------------------------------------------------------------------

/// **A peer's close while parked completes the operation as `{error, closed}` — once.** The C's
/// `tcp_recv_closed` answers the pending operation with `async_error_am(am_closed)`
/// (`inet_drv.c:12829`) and tears the descriptor down under the default `{exit_on_close,true}`
/// (`:12824`-`:12827`), so `GETSTATUS` reports `CLOSED` afterwards and a later receive is the bare
/// `[0]enotconn` — the `recv` ladder's `!IS_CONNECTED` arm, with its one-shot already spent
/// (`:12279`-`:12288`).
#[test]
fn a_peer_close_while_parked_completes_as_closed_once() {
    let (mut port, peer) = linked();
    assert_eq!(arm_receive(&mut port, INET_INFINITY), 0);
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "parked before the peer goes away"
    );

    drop(peer);
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Closed))],
        "the parked operation ends as {{error, closed}}"
    );
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "a teardown answers each pending operation exactly once"
    );
    assert_eq!(
        state_code(&mut port),
        INET_STATE_CLOSED as u8,
        "the passive socket was torn down with the close (`tcp_desc_close`, `:12824`)"
    );
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        ctl_xerror("enotconn"),
        "a later receive on the dead connection is the bare `enotconn`"
    );
}

/// **Closing the port while parked completes the operation as `{error, closed}` — once.** The
/// teardown drain is `Driver::stop` → `async_error_am_all(am_closed)` (`inet_drv.c:2597`-`:2608`,
/// called from `packet_inet_stop` `:14548`), which the registry harvests so the owner still sees it.
///
/// Afterwards the **port number is gone** (the registry drops the port at `close`), so a later
/// receive is the `badarg` every control call on a closed port answers — the same thing
/// `erlang:port_control/3` raises on the oracle once `erlang:port_close/1` has run.
#[test]
fn a_port_close_while_parked_completes_as_closed_once() {
    let (mut port, _peer) = linked();
    assert_eq!(arm_receive(&mut port, INET_INFINITY), 0);
    assert!(
        park_window(&mut port, PARK_WINDOW).is_empty(),
        "parked before the port is closed"
    );

    port.close();
    assert!(!port.contains(), "the port is gone");
    assert_eq!(
        async_events(&port.completions()),
        vec![(0, InetAsyncResult::Error(InetReason::Closed))],
        "the teardown answers the parked operation"
    );
    assert!(
        port.completions().is_empty(),
        "and the harvest is drained exactly once"
    );
    assert!(
        matches!(
            port.try_control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
            Err(beam_common::BeamError::BadArg)
        ),
        "a closed port answers the oracle's `badarg`, never a fabricated reply"
    );
}

// ---------------------------------------------------------------------------
// The oracle cross-check
// ---------------------------------------------------------------------------

/// One row of the compared dump: how the park is driven, and how this floor's answer is written.
///
/// The rows are compared as **text**, which is why both sides have a renderer: the `erl` probe
/// prints `~w` of the same terms ([`escript_stdout`]), and [`render`] spells the Rust side's reply
/// bytes / completion the way `~w` would.
fn render_reply(bytes: &[u8]) -> String {
    let inner = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

/// The way the oracle's `prim_inet` probe prints an **arming** reply. `prim_inet:async_recv/3`
/// (`erts/preloaded/src/prim_inet.erl:820`) does not hand its caller the raw `port_control` bytes:
/// it turns `[1] ++ <<Id:16/big>>` into `{ok, Id}` and anything else into `{error, Reason}` with the
/// reason read out of the reply's name. The probe drives the park through *that* function, so the
/// Rust side must speak the same vocabulary for the two dumps to be comparable row for row.
fn render_arm(reply: &[u8]) -> String {
    match reply.split_first() {
        Some((&INET_REP_OK, id)) => {
            let [hi, lo] = <[u8; 2]>::try_from(id)
                .unwrap_or_else(|_| panic!("the arming reply is `[1] ++ <<Id:16/big>>`, got {reply:?}"));
            format!("{{ok,{}}}", u16::from_be_bytes([hi, lo]))
        }
        Some((&INET_REP_ERROR, name)) => format!("{{error,{}}}", String::from_utf8_lossy(name)),
        None => panic!("a control reply is never empty: {reply:?}"),
    }
}

/// `~w` of the `{inet_async, Port, Id, Status}` a receive produces, with the id **rebased** on the
/// first receive (see [`the_passive_park_sequence_agrees_with_the_erl_oracle`]) and the port
/// dropped (the two sides own different ports).
fn render(id: u16, result: &InetAsyncResult) -> String {
    let status = match result {
        // `{mode, list}` is the C's default (`INET_MODE_LIST`), so the payload is a flat int list;
        // a `{mode, binary}` read would render as `<<"…">>` and is the runtime's rendering to
        // choose (`inet_async_data`, `inet_drv.c:3162`-`:3190`).
        InetAsyncResult::Data(read) => {
            let flat = render_reply(&read.flattened());
            format!("{{ok,{flat}}}")
        }
        InetAsyncResult::Error(reason) => format!("{{error,{}}}", reason.atom_name()),
        other => panic!("the park produced an unexpected completion: {other:?}"),
    };
    format!("{{{id},{status}}}")
}

/// The `prim_inet` probe the oracle rows come from: a raw
/// `open_port({spawn_driver, "tcp_inet"}, [binary])` port, `open`ed and `connect`ed to a listening
/// `gen_tcp` socket, then the passive receive sequence driven through `prim_inet:async_recv/3`.
///
/// Two properties of that construction are load-bearing and are stated here rather than
/// rediscovered:
///
/// * the raw `INET_REQ_CONNECT`(3) is **asynchronous on the oracle** (it answers
///   `[1] ++ <<Id:16>>` and completes later), so it consumes the descriptor's own async id `0`;
///   the completion is drained before the receive rows and every later id is **rebased** on the
///   first receive's id, so the two sides compare `0, 2, 3, 4` rather than `1, 3, 4, 5`;
/// * `prim_inet:async_recv/3` takes a **millisecond** budget where `recv/3` takes `infinity`, so
///   the infinite operand is the C's `INET_INFINITY` as `-1` (`prim_inet.erl:2777`).
const ORACLE_PROBE: &str = r#"#!/usr/bin/env escript
main(_) ->
    J = fun(R) -> io:format("~w|", [R]) end,
    One = fun() ->
                  receive {inet_async, _, Id, Status} -> {Id, Status}
                  after 2000 -> timeout_msg
                  end
          end,
    None = fun() ->
                   receive {inet_async, _, Id, Status} -> {unexpected, Id, Status}
                   after 200 -> none
                   end
           end,
    {ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]),
    {ok,{_,P0}} = inet:sockname(L),
    P = open_port({spawn_driver,"tcp_inet"},[binary]),
    OPEN = 1 + 16#03f1a300,
    CONNECT = 3 + 16#03f1a300,
    STATUS = 16 + 16#03f1a300,
    [1] = erlang:port_control(P, OPEN, <<1,1,0>>),
    erlang:port_control(P, CONNECT, <<255,255,255,255,4,(P0 bsr 8),(P0 band 255)>>),
    Wait = fun F(0) -> io:format(standard_error,"the raw connect never completed~n",[]),
                        erlang:halt(1);
                F(N) -> case erlang:port_control(P,STATUS,<<>>) of
                            [1,0,0,0,5] -> ok;
                            _ -> timer:sleep(1), F(N-1)
                        end
             end,
    Wait(2000),
    {ok,A} = gen_tcp:accept(L, 5000),
    {0, ok} = One(),
    J(erlang:port_control(P, STATUS, <<>>)),
    {ok, Base} = prim_inet:async_recv(P,0,-1),
    Rel = fun(Id) -> Id - Base end,
    J({ok, Rel(Base)}),
    J(None()),
    J(prim_inet:async_recv(P,0,-1)),
    ok = gen_tcp:send(A,<<"abc">>),
    {I1, S1} = One(), J({Rel(I1), S1}),
    J(None()),
    {ok, Z} = prim_inet:async_recv(P,0,0), J({ok, Rel(Z)}),
    {I2, S2} = One(), J({Rel(I2), S2}),
    J(None()),
    {ok, F} = prim_inet:async_recv(P,0,50), J({ok, Rel(F)}),
    {I3, S3} = One(), J({Rel(I3), S3}),
    ok = gen_tcp:close(A),
    {ok, C} = prim_inet:async_recv(P,0,-1), J({ok, Rel(C)}),
    {I4, S4} = One(), J({Rel(I4), S4}),
    J(None()),
    J(try prim_inet:async_recv(P,0,-1) catch _:_ -> raised end),
    J(try erlang:port_control(P,STATUS,<<>>) catch _:_ -> raised end),
    io:format("~n").
"#;

/// **The whole park sequence, row for row against the system `erl`.**
///
/// Sixteen rows, driven identically on both sides: the connected state, the first (parked) receive
/// and its id, the silence while parked, the `ealready` of a second receive, the data that ends the
/// park, the silence afterwards, the zero and 50 ms deadlines, the park that the peer's close ends,
/// the close's own `{error, closed}`, the silence after it, a later receive's `enotconn`, and the
/// final `CLOSED` status.
///
/// **Skips loudly** when `escript` is not on `PATH` or does not answer within its budget.
#[test]
fn the_passive_park_sequence_agrees_with_the_erl_oracle() {
    // ---- this floor's rows -------------------------------------------------
    let (mut port, mut peer) = linked();
    let mut rust: Vec<String> = Vec::new();

    rust.push(render_reply(&port.control(&op(INET_REQ_GETSTATUS), &[])));
    assert_eq!(state_code(&mut port), INET_STATE_CONNECTED as u8);

    let base = arm_receive(&mut port, INET_INFINITY);
    rust.push(format!("{{ok,{base}}}"));
    rust.push(
        match park_window(&mut port, PARK_WINDOW).as_slice() {
            [] => "none".to_owned(),
            other => panic!("the park must be silent, got {other:?}"),
        },
    );
    rust.push(match port.try_control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)) {
        Ok(reply) => render_arm(&reply),
        Err(err) => panic!("a refused enqueue is a reply, not a raise: {err:?}"),
    });

    peer.write_all(b"abc").expect("the peer writes");
    for (id, result) in await_events(&mut port, 1) {
        rust.push(render(id, &result));
    }
    rust.push(
        match park_window(&mut port, PARK_WINDOW).as_slice() {
            [] => "none".to_owned(),
            other => panic!("one write is one completion, got {other:?}"),
        },
    );

    let zero = arm_receive(&mut port, 0);
    rust.push(format!("{{ok,{zero}}}"));
    for (id, result) in await_events(&mut port, 1) {
        rust.push(render(id, &result));
    }
    rust.push(
        match park_window(&mut port, PARK_WINDOW).as_slice() {
            [] => "none".to_owned(),
            other => panic!("a timeout is reported once, got {other:?}"),
        },
    );

    let finite = arm_receive(&mut port, 50);
    rust.push(format!("{{ok,{finite}}}"));
    for (id, result) in await_events(&mut port, 1) {
        rust.push(render(id, &result));
    }

    let closing = arm_receive(&mut port, INET_INFINITY);
    rust.push(format!("{{ok,{closing}}}"));
    drop(peer);
    for (id, result) in await_events(&mut port, 1) {
        rust.push(render(id, &result));
    }
    rust.push(
        match park_window(&mut port, PARK_WINDOW).as_slice() {
            [] => "none".to_owned(),
            other => panic!("the close is reported once, got {other:?}"),
        },
    );
    rust.push(match port.try_control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)) {
        Ok(reply) => render_arm(&reply),
        Err(err) => panic!("a later receive is a reply, not a raise: {err:?}"),
    });
    rust.push(render_reply(&port.control(&op(INET_REQ_GETSTATUS), &[])));

    // The ids the rows must carry: the first receive is 0, the refused enqueue takes 1, and the
    // three later operations are 2, 3 and 4 — on **both** sides, because the oracle's dump is
    // rebased on the first receive's id (the raw connect consumed the descriptor's id 0).
    let expected: Vec<String> = vec![
        format!("[{INET_REP_OK},0,0,0,{INET_STATE_CONNECTED}]"), // GETSTATUS: connected
        "{ok,0}".to_owned(),                                    // the parked receive
        "none".to_owned(),                                      // still parked
        "{error,ealready}".to_owned(), // the single receive slot is taken
        "{0,{ok,[97,98,99]}}".to_owned(),                       // the peer's three bytes
        "none".to_owned(),                                      // nothing more
        "{ok,2}".to_owned(),                                    // zero deadline (id 1 was spent)
        "{2,{error,timeout}}".to_owned(),                       // …expired
        "none".to_owned(),                                      // once
        "{ok,3}".to_owned(),                                    // 50 ms deadline
        "{3,{error,timeout}}".to_owned(),                       // …expired
        "{ok,4}".to_owned(),                                    // parked before the peer's close
        "{4,{error,closed}}".to_owned(),                        // …answered by it
        "none".to_owned(),                                      // once
        "{error,enotconn}".to_owned(), // the connection is gone
        format!("[{INET_REP_OK},0,0,0,{INET_STATE_CLOSED}]"),    // GETSTATUS: closed
    ];
    assert_eq!(rust, expected, "this floor's park rows");

    // ---- the oracle's rows -------------------------------------------------
    let Some(dump) = escript_stdout("passive_park", ORACLE_PROBE, &[]) else {
        eprintln!("SKIP: no `escript` on PATH — skipping the passive-park oracle cross-check");
        return;
    };
    let oracle: Vec<String> = dump
        .split('|')
        .map(str::trim)
        .filter(|row| !row.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        oracle.len(),
        expected.len(),
        "the oracle answered every row: {dump:?}"
    );
    for (index, (oracle_row, expected_row)) in oracle.iter().zip(&expected).enumerate() {
        assert_eq!(
            oracle_row, expected_row,
            "park row #{index} disagrees (dump: {dump:?})"
        );
    }
}
