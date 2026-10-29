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
//! The `{send_timeout, …}`/`{send_timeout_close, …}` tests: the deadline arithmetic of the state
//! machine, the port-timer arbitration with a queued async operation, and the observable rows over
//! a **real** loopback socket — each row's shape cross-checked against the `escript` oracle.
//!
//! Two layers:
//!
//! * the **pure** state machine ([`SendTimeout`]) — arm/`next_deadline`/`take_expired`, the
//!   one-shot guard, the infinite and zero forms, `cancel`/`begin_command`. No socket, no clock
//!   wait: every "the deadline passed" assertion hands `take_expired` the deadline itself.
//! * the **driver** rows — a loopback pair whose client is forced to stall (a narrow `SO_SNDBUF`
//!   and a payload the kernel cannot take while the peer reads nothing), driven through
//!   [`Rig`] exactly as the runtime drives it. Every wait is a condition under [`HANG_GUARD`],
//!   never a fixed sleep.
//!
//! The oracle leg ([`the_oracle_reports_the_measured_send_timeout_rows`]) runs the measured table
//! against the system `escript` and skips loudly when it is absent.
use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::op_queue::async_tests::{linked, recv_operand};
use super::opcode_tests::{escript_stdout, op, Rig};
use super::opt::{
    INET_LOPT_ACTIVE, INET_LOPT_EXITONCLOSE, INET_LOPT_TCP_SEND_TIMEOUT,
    INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, INET_OPT_SNDBUF,
};
use super::wire::ctl_ok;
use super::send_timeout::SendTimeout;
use crate::completion::{CompletionEvent, InetReason, InetReply};

/// `TCP_REQ_RECV` (`inet_drv.c:110`) — used to queue an async deadline beside the send one.
const TCP_REQ_RECV: u32 = 42;
/// `INET_REQ_SETOPTS` (`inet_drv.c:597`).
const INET_REQ_SETOPTS: u32 = 7;
/// The bound on every condition wait below. The wait ends the instant its condition holds, so this
/// only stops a broken driver from hanging the suite.
const HANG_GUARD: Duration = Duration::from_secs(20);
/// How long one condition-wait iteration sleeps between rounds.
const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// A payload the loopback pair cannot absorb while the peer reads nothing, so the first `send(2)`
/// leaves a strict remainder on the driver's output queue — the stall the timer is armed for.
const STALL_PAYLOAD: usize = 16 * 1024 * 1024;
/// The `prim_inet` caller tag as it looks on the wire (`term_to_binary(Mref, [local])`, which
/// always starts with the ETF `VERSION_MAGIC` 131) — the tag the timeout reply must echo.
const CALLER_REF_TAG: &[u8] = &[131, 100, 0, 8, b'r', b'e', b'f', b'0'];

// ---------------------------------------------------------------------------
// The pure state machine
// ---------------------------------------------------------------------------

/// **The deadline is the only thing that fires it.** A timer armed with a delay has a deadline in
/// the future, `take_expired` before it answers nothing and leaves the timer armed, and the same
/// call at the deadline hands back exactly the stalled command and leaves the timer `Fired`.
#[test]
fn a_deadline_fires_once_and_only_when_due() {
    let mut timer = SendTimeout::new();
    let before = Instant::now();
    timer.arm(Some(CALLER_REF_TAG.to_vec()), 250);
    let deadline = timer.next_deadline().expect("an armed timer has a deadline");
    assert!(deadline >= before + Duration::from_millis(250));
    assert!(timer.is_armed(), "the timer is armed, not fired");
    assert!(
        timer.take_expired(Instant::now()).is_none(),
        "a deadline in the future answers nothing"
    );
    assert!(timer.is_armed(), "and leaves the timer armed");
    let fired = timer
        .take_expired(deadline)
        .expect("the deadline itself is due");
    assert_eq!(fired.caller_ref.as_deref(), Some(CALLER_REF_TAG));
    assert_eq!(fired.deadline, deadline);
    assert!(timer.has_fired(), "the one-shot is spent");
    assert!(
        timer.next_deadline().is_none(),
        "a fired timer declares no deadline, so the port clears its TIMEOUT task"
    );
    assert!(
        timer.take_expired(deadline).is_none(),
        "a second call answers nothing: the caller is answered exactly once"
    );
}

/// **Arming is a one-shot for the whole stall.** A second `arm` — the re-driven `OUTPUT` task that
/// attempts the same remainder again — neither re-points the deadline nor arms a fired timer, so a
/// stall cannot push its own deadline out and cannot answer twice.
#[test]
fn arming_is_a_one_shot_for_the_whole_stall() {
    let mut timer = SendTimeout::new();
    timer.arm(Some(CALLER_REF_TAG.to_vec()), 250);
    let first = timer.next_deadline().expect("armed");
    timer.arm(Some(b"other".to_vec()), 5_000);
    assert_eq!(
        timer.next_deadline(),
        Some(first),
        "a re-arm of the same stall keeps the first deadline and the first caller"
    );
    assert_eq!(
        timer.take_expired(first).expect("due").caller_ref,
        Some(CALLER_REF_TAG.to_vec()),
        "the addressee is the command that armed it"
    );
    timer.arm(Some(b"other".to_vec()), 5_000);
    assert!(
        !timer.is_armed(),
        "a fired stall is not re-armed: the queue has not drained yet"
    );
}

/// **`{send_timeout, infinity}` arms nothing and `{send_timeout, 0}` is due at once** — the C's
/// `if (desc->send_timeout != INET_INFINITY)` guard (`inet_drv.c:13785`) and its zero-timeout
/// shortcut (`:13776`/`:13888`).
#[test]
fn an_infinite_timeout_arms_nothing_and_a_zero_one_is_due_at_once() {
    let mut infinite = SendTimeout::new();
    infinite.arm(Some(CALLER_REF_TAG.to_vec()), -1);
    assert!(!infinite.is_armed(), "`INET_INFINITY` arms no timer");
    assert!(infinite.next_deadline().is_none());

    let mut immediate = SendTimeout::new();
    immediate.arm(Some(CALLER_REF_TAG.to_vec()), 0);
    let deadline = immediate.next_deadline().expect("a zero timeout arms now");
    assert!(
        deadline <= Instant::now(),
        "the deadline is the arming instant, not a delay from it"
    );
    assert!(
        immediate.take_expired(Instant::now()).is_some(),
        "so the very next port round answers it"
    );
}

/// **`cancel` drops the deadline, `begin_command` opens a new one-shot for the next command.**
/// The two are the drain and the next-command edges of the state machine: a drained queue owes no
/// timeout, and a stall that already fired belongs to the command that was answered.
#[test]
fn cancel_clears_the_deadline_and_begin_command_opens_a_new_one() {
    let mut timer = SendTimeout::new();
    timer.arm(Some(CALLER_REF_TAG.to_vec()), 250);
    let deadline = timer.next_deadline().expect("armed");
    timer.cancel();
    assert!(!timer.is_armed() && !timer.has_fired(), "the timer is idle");
    assert_eq!(timer.next_deadline(), None);
    assert!(timer.take_expired(deadline).is_none(), "nothing fires");

    timer.arm(Some(CALLER_REF_TAG.to_vec()), 0);
    let _fired = timer.take_expired(Instant::now()).expect("due at once");
    assert!(timer.has_fired());
    timer.begin_command();
    assert!(!timer.has_fired(), "the next command starts a fresh one-shot");
    timer.arm(Some(b"next".to_vec()), 250);
    assert!(timer.is_armed(), "and may arm its own deadline");
    let deadline = timer.next_deadline().expect("armed");
    let fired = timer.take_expired(deadline).expect("due");
    assert_eq!(
        fired.caller_ref.as_deref(),
        Some(&b"next"[..]),
        "the new command's own addressee is answered"
    );
}

// ---------------------------------------------------------------------------
// Driving a real loopback socket
// ---------------------------------------------------------------------------

/// One five-byte socket-option record, `[code, value_be32]` (`inet_set_opts`, `inet_drv.c:6908`).
fn option_record(code: u8, value: i32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}

/// Sets one socket option through `INET_REQ_SETOPTS`(7), asserting the `[1]` reply.
fn set_option(port: &mut Rig, code: u8, value: i32) {
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &option_record(code, value)),
        ctl_ok(),
        "INET_REQ_SETOPTS({code}) is accepted"
    );
}

/// A `tcp_inet` port over a loopback pair whose send buffer is **narrow**, so a large command
/// stalls: the far end stays here (the connection is held open) and reads nothing until a test
/// hands it to a reader thread.
fn stalled_pair() -> (Rig, std::net::TcpStream) {
    let (port, peer) = linked();
    let mut port = port;
    set_option(&mut port, INET_OPT_SNDBUF, 4096);
    (port, peer)
}

/// Queues one command carrying [`CALLER_REF_TAG`] and runs its round — the `gen_tcp:send/2` shape:
/// `[n:be16][tag][payload]` (`init_caller`, `inet_drv.c:1820`).
fn send_tagged(port: &mut Rig, payload: &[u8]) {
    send_tagged_with(port, CALLER_REF_TAG, payload);
}

/// The same command with an explicit caller tag.
fn send_tagged_with(port: &mut Rig, tag: &[u8], payload: &[u8]) {
    let mut command = (tag.len() as u16).to_be_bytes().to_vec();
    command.extend_from_slice(tag);
    command.extend_from_slice(payload);
    port.command(&command).expect("the command is queued");
}

/// The port's output-queue depth (the C's `driver_sizeq`) — what decides whether a command
/// stalled.
fn output_len(port: &mut Rig) -> usize {
    port.registry.get(port.id).expect("a live port").output_len()
}

/// Every `{inet_reply, …}` the last round staged, as `(caller_ref, reply)`.
fn replies(port: &mut Rig) -> Vec<(Option<Vec<u8>>, InetReply)> {
    port.completions()
        .into_iter()
        .filter_map(|completion| match completion.event {
            CompletionEvent::InetReply { caller_ref, reply } => Some((caller_ref, reply)),
            _ => None,
        })
        .collect()
}

/// Runs port rounds until `want` `{inet_reply, …}` completions have arrived, returning them — the
/// condition-under-a-guard wait every row below uses.
fn await_replies(port: &mut Rig, want: usize) -> Vec<(Option<Vec<u8>>, InetReply)> {
    let guard = Instant::now() + HANG_GUARD;
    let mut collected = Vec::new();
    while collected.len() < want {
        port.round().expect("a port-task round");
        collected.extend(replies(port));
        if collected.len() >= want {
            break;
        }
        assert!(
            Instant::now() < guard,
            "only {} of {want} `{{inet_reply, …}}` completions arrived within {HANG_GUARD:?}: \
             {collected:?}",
            collected.len()
        );
        thread::sleep(POLL_INTERVAL);
    }
    collected
}

/// **A stalled command arms the one-shot, and the queue draining cancels it.**
///
/// The kernel cannot take [`STALL_PAYLOAD`] with a 4 KiB send buffer and a peer that reads
/// nothing, so the first command leaves a strict remainder queued: [`Rig::next_timeout`] then
/// declares the `{send_timeout, …}` deadline (which the port mirrors into its single `TIMEOUT`
/// task), and the command's caller is answered `ok` only when the bytes are really off the queue.
/// Once the peer drains, the queue empties, the caller is answered and the deadline is gone — the
/// C's low-watermark `cancel_multi_timer(&tcp_inet_send_timeout)` (`inet_drv.c:14006`).
#[test]
fn a_stalled_write_arms_the_one_shot_and_the_drain_cancels_it() {
    let (mut port, peer) = stalled_pair();
    // A deadline far beyond the drain below: this row pins the *arm* and the *cancel*, not the
    // expiry (which the rows further down pin), so the drain must never race the timer.
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT, 60_000);
    assert_eq!(
        port.next_timeout(),
        None,
        "an idle port declares no deadline"
    );
    send_tagged(&mut port, &vec![b'a'; STALL_PAYLOAD]);
    let queued = output_len(&mut port);
    assert!(
        queued > 0 && queued < STALL_PAYLOAD,
        "the kernel took a strict part of the payload and left {queued} byte(s) queued"
    );
    let armed = port
        .next_timeout()
        .expect("a stalled command declares the `{send_timeout, …}` deadline");
    assert!(
        armed > Instant::now() + Duration::from_secs(1),
        "the 60 s deadline is far in the future, not the past"
    );
    assert!(
        replies(&mut port).is_empty(),
        "nothing is answered while the remainder is queued"
    );

    // Let the peer read, and pump the reactor until the queue drains — the write readiness the
    // driver arms re-drives `write_pending` to completion.
    let mut reader = peer.try_clone().expect("a second descriptor for the far end");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut got = vec![0u8; STALL_PAYLOAD];
        let _ = tx.send(reader.read_exact(&mut got));
    });
    let guard = Instant::now() + HANG_GUARD;
    while output_len(&mut port) > 0 {
        let events = port
            .poller
            .poll(Some(Duration::from_millis(20)))
            .expect("the poll set");
        port.registry
            .handle_events(&mut port.poller, &events)
            .expect("a reactor round");
        assert!(
            Instant::now() < guard,
            "the queued remainder never drained ({} byte(s) left)",
            output_len(&mut port)
        );
    }
    assert!(
        rx.recv_timeout(HANG_GUARD).expect("the reader reports").is_ok(),
        "the peer read the whole payload"
    );
    handle.join().expect("the reader thread");
    assert_eq!(
        replies(&mut port),
        vec![(Some(CALLER_REF_TAG.to_vec()), InetReply::Ok)],
        "the drained command answers its caller `ok`"
    );
    assert_eq!(
        port.next_timeout(),
        None,
        "and the deadline is gone with the queue"
    );
}

/// **`next_timeout` reports the earlier of the two deadlines the port's single `TIMEOUT` task
/// carries.** A queued `TCP_REQ_RECV` with a short timeout arms an async deadline beside the
/// stalled command's send deadline; the port must be woken at the earlier one, or the receive's
/// `{error, timeout}` would be answered late — and a `None` would clear the task the receive needs
/// (`Port::refresh_timeout` mirrors this answer).
#[test]
fn next_timeout_reports_the_earlier_of_the_recv_and_send_deadlines() {
    let (mut port, _peer) = stalled_pair();
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT, 60_000);
    send_tagged(&mut port, &vec![b'a'; STALL_PAYLOAD]);
    let send_deadline = port.next_timeout().expect("the stalled send declares a deadline");
    assert!(output_len(&mut port) > 0, "the send really stalled");

    // A 20 ms receive on a socket with no data: it cannot complete, so it arms its own deadline.
    let armed = port.control(&op(TCP_REQ_RECV), &recv_operand(20, 0));
    assert_eq!(armed[0], 1, "the arming reply is `[1] ++ id`: {armed:?}");
    let both = port
        .next_timeout()
        .expect("both deadlines are declared while both are pending");
    assert!(
        both < send_deadline,
        "the 20 ms receive deadline ({both:?}) is earlier than the send deadline \
         ({send_deadline:?})"
    );
}

/// **A due send deadline answers the issuing caller `{error, timeout}` exactly once.**
///
/// With `{send_timeout, 40}` and a stalled command, the port's `TIMEOUT` task reaches
/// [`crate::Driver::timeout`] → [`super::TcpInetDriver::send_timeout_fired`], which answers the command's
/// caller tag `{error, timeout}` (the C's `inet_reply_error_am(desc, am_timeout)`,
/// `inet_drv.c:12484`) and stages nothing else: the queue is still non-empty, so the drain that
/// follows can never answer the same tag again.
#[test]
fn a_due_send_deadline_answers_the_issuing_caller_once() {
    let (mut port, _peer) = stalled_pair();
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT, 40);
    send_tagged(&mut port, &vec![b'a'; STALL_PAYLOAD]);
    let timed_out = await_replies(&mut port, 1);
    assert_eq!(
        timed_out,
        vec![(
            Some(CALLER_REF_TAG.to_vec()),
            InetReply::Error(InetReason::Timeout)
        )],
        "the stalled command's own caller is answered `{{error, timeout}}`"
    );
    assert!(
        output_len(&mut port) > 0,
        "`{{send_timeout_close, false}}` keeps the socket, so the remainder is still queued"
    );
    // Nothing further arrives: the tag was spent by the timeout reply, and the port has no other
    // caller to answer (the next command's tag is a new one).
    for _ in 0..20 {
        port.round().expect("a port-task round");
        assert!(
            replies(&mut port).is_empty(),
            "a fired stall answers once, never twice"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

/// **`{send_timeout_close, true}` on a passive socket answers the caller `{error, timeout}` and
/// then takes the descriptor down without signalling the owner** — the C's passive
/// `tcp_desc_close` (`inet_drv.c:12521`: the queued output and the descriptor go, the **port
/// stays**, its next command the ladder's `enotconn`), not the active `driver_exit` arm.
///
/// The reply is staged **before** the teardown (the delivery seams refuse once a failure latched),
/// the teardown latches no failure at all, and a **second** stalled command neither re-arms the
/// one-shot nor double-latches anything: it is answered the ladder's `{error, enotconn}` for its
/// own caller tag.
#[test]
fn send_timeout_close_answers_the_caller_and_the_next_command_is_enotconn() {
    let (mut port, _peer) = stalled_pair();
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT, 40);
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1);
    send_tagged(&mut port, &vec![b'a'; STALL_PAYLOAD]);
    assert_eq!(
        await_replies(&mut port, 1),
        vec![(
            Some(CALLER_REF_TAG.to_vec()),
            InetReply::Error(InetReason::Timeout)
        )],
        "the stalled command's caller is answered `{{error, timeout}}` before the teardown"
    );
    assert_eq!(
        output_len(&mut port),
        0,
        "`tcp_desc_close` drops the queued remainder (`tcp_clear_output`, `inet_drv.c:12786`)"
    );
    assert_eq!(
        port.next_timeout(),
        None,
        "and no deadline is left behind"
    );
    assert!(
        port.registry.take_failures().is_empty(),
        "the passive arm signals nothing: `{{send_timeout_close,true}}` + `{{exit_on_close,false}}` \
         is the C's `tcp_desc_close`, not `driver_exit`"
    );
    assert!(
        port.contains(),
        "the port itself survives the passive teardown (`erl_inet_close` keeps it)"
    );
}

/// **A command that completes before the deadline answers `ok` and never times out.**
///
/// The peer reads continuously, so the payload is off the queue at issue: nothing arms, the caller
/// is answered `ok`, and a wait well past the deadline produces no second reply — the C cancels the
/// timer at the low watermark before it can fire.
#[test]
fn a_command_that_completes_before_the_deadline_never_times_out() {
    let (mut port, peer) = stalled_pair();
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT, 40);
    const SHORT: &[u8] = b"a payload the kernel takes at once";
    let mut reader = peer.try_clone().expect("a second descriptor for the far end");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut got = vec![0u8; SHORT.len()];
        let read = reader.read_exact(&mut got);
        let _ = tx.send((read, got));
    });
    send_tagged(&mut port, SHORT);
    assert_eq!(
        replies(&mut port),
        vec![(Some(CALLER_REF_TAG.to_vec()), InetReply::Ok)],
        "a command wholly written answers `ok` in the same round"
    );
    assert_eq!(
        port.next_timeout(),
        None,
        "an immediately drained queue arms no deadline"
    );
    // Watch past the deadline: the reply above is the only one.
    let until = Instant::now() + Duration::from_millis(40) + Duration::from_millis(60);
    while Instant::now() < until {
        port.round().expect("a port-task round");
        assert!(
            replies(&mut port).is_empty(),
            "a command answered `ok` is never answered again"
        );
        thread::sleep(POLL_INTERVAL);
    }
    let (read, got) = rx.recv_timeout(HANG_GUARD).expect("the reader reports");
    assert!(read.is_ok(), "the peer read the payload");
    handle.join().expect("the reader thread");
    assert_eq!(got, SHORT, "the peer read exactly the payload");
}

/// **An active socket under `{send_timeout_close, true}` and the default `{exit_on_close, true}`
/// takes the C's `driver_exit` arm** (`inet_drv.c:12500`-`:12509`): the caller is answered
/// `{error, timeout}` first and the failure that follows signals the owner, unlike the passive
/// `tcp_desc_close` arm above.
#[test]
fn an_active_socket_tears_down_through_the_exit_arm() {
    let (mut port, _peer) = stalled_pair();
    set_option(&mut port, INET_LOPT_ACTIVE, 1);
    set_option(&mut port, INET_LOPT_EXITONCLOSE, 1);
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT, 40);
    set_option(&mut port, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1);
    send_tagged(&mut port, &vec![b'a'; STALL_PAYLOAD]);
    assert_eq!(
        await_replies(&mut port, 1),
        vec![(
            Some(CALLER_REF_TAG.to_vec()),
            InetReply::Error(InetReason::Timeout)
        )],
        "the caller is answered before the teardown"
    );
    let guard = Instant::now() + HANG_GUARD;
    let mut failures = port.registry.take_failures();
    while failures.is_empty() {
        port.round().expect("a port-task round");
        failures = port.registry.take_failures();
        assert!(
            Instant::now() < guard,
            "the active `{{send_timeout_close, true}}` arm never latched a failure"
        );
        thread::sleep(POLL_INTERVAL);
    }
    assert_eq!(failures.len(), 1, "exactly one latch: {failures:?}");
    assert!(
        !failures[0].failure.signals_owner,
        "`driver_exit` unlinks the owner rather than signalling it: {failures:?}"
    );
    assert!(!port.contains(), "the port itself is reaped");
}

// ---------------------------------------------------------------------------
// The oracle leg
// ---------------------------------------------------------------------------

/// The probe driving the three measured rows, one line each:
/// `label|results|ms|getopts-values|peername-tag|sockname-tag`.
///
/// The payload is 4 MiB and the client's `SO_SNDBUF` is 4 KiB, so a send the peer never reads
/// stalls; each row uses a **fresh** loopback pair, so the rows cannot influence one another.
const ORACLE_PROBE: &str = r#"
-mode(compile).
-define(PAYLOAD, 4 * 1024 * 1024).

pair() ->
    {ok, L} = gen_tcp:listen(0, [binary, {active, false}, {reuseaddr, true}]),
    {ok, Port} = inet:port(L),
    Self = self(),
    spawn_link(fun() ->
        {ok, S} = gen_tcp:accept(L),
        Self ! {accepted, S},
        receive stop -> ok end
    end),
    {ok, C} = gen_tcp:connect({127,0,0,1}, Port, [binary, {active,false}, {sndbuf,4096}]),
    _S = receive {accepted, X} -> X end,
    C.

ms(T0, T1) -> (T1 - T0) div 1000.

row(Label, Opts) ->
    C = pair(),
    ok = inet:setopts(C, Opts),
    P = binary:copy(<<"a">>, ?PAYLOAD),
    {Results, Times} =
        lists:unzip([begin
                         T0 = erlang:monotonic_time(microsecond),
                         R = gen_tcp:send(C, P),
                         {R, ms(T0, erlang:monotonic_time(microsecond))}
                     end || _ <- [1,2,3]]),
    {ok, Values} = inet:getopts(C, [send_timeout, send_timeout_close]),
    Peer = case inet:peername(C) of {ok, _} -> ok; {error, E1} -> {error, E1} end,
    Sock = case inet:sockname(C) of {ok, _} -> ok; {error, E2} -> {error, E2} end,
    %% `~w`, not `~p`: the default `~p` line width would split a term across lines.
    io:format("~s|~w|~s|~w|~w|~w~n",
              [Label, Results,
               lists:join(",", [integer_to_list(T) || T <- Times]),
               [V || {_, V} <- Values], Peer, Sock]),
    %% Never close a stalled client here: the oracle's close waits for the queued
    %% output to drain, which is exactly what this probe makes impossible.
    ok.

main(_) ->
    row("plain200", [{send_timeout, 200}]),
    row("close200", [{send_timeout, 200}, {send_timeout_close, true}]),
    row("zero",     [{send_timeout, 0}]),
    halt(0).
"#;

/// One parsed probe row.
#[derive(Debug)]
struct OracleRow {
    results: String,
    ms: Vec<u64>,
    getopts: String,
    peer: String,
    sock: String,
}

/// Runs [`ORACLE_PROBE`] and parses its rows by label.
fn oracle_rows() -> Option<Vec<(String, OracleRow)>> {
    let stdout = escript_stdout("send_timeout", ORACLE_PROBE, &[])?;
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('|').collect();
        assert_eq!(
            fields.len(),
            6,
            "the probe prints `label|results|ms|getopts|peer|sock`, got {line:?}"
        );
        let ms = fields[2]
            .split(',')
            .map(|field| field.parse::<u64>().expect("a millisecond count"))
            .collect();
        rows.push((
            fields[0].to_owned(),
            OracleRow {
                results: fields[1].to_owned(),
                ms,
                getopts: fields[3].to_owned(),
                peer: fields[4].to_owned(),
                sock: fields[5].to_owned(),
            },
        ));
    }
    assert_eq!(rows.len(), 3, "the probe drives three rows: {rows:?}");
    Some(rows)
}

/// **The measured `{send_timeout, …}` / `{send_timeout_close, …}` rows, as the oracle answers
/// them.**
///
/// Measured on OTP 29.1.2 / erts-17.1.1 (the host oracle), a fresh loopback pair per row, a 4 KiB
/// client `SO_SNDBUF` and a 4 MiB payload the peer never reads:
///
/// ```text
/// row       first send  second send        third send        getopts            peername        sockname
/// plain200  ok (~1 ms)  {error,timeout} ~200 ms  {error,timeout} ~200 ms  [200,false]  {ok,…}   {ok,…}
/// close200  ok (~1 ms)  {error,timeout} ~200 ms  {error,enotconn} ~1 ms   [200,true]   {error,enotconn}  {error,ebadf}
/// zero      ok (~1 ms)  {error,timeout} ~0 ms    {error,timeout} ~0 ms    [0,false]    {ok,…}   {ok,…}
/// ```
///
/// The invariant behind the numbers: the 200 ms rows **wait** for their timeout (the C's
/// `tcp_inet_send_timeout`, `inet_drv.c:12458`), while `{send_timeout, 0}` answers at once (the
/// zero-timeout shortcut, `:13776`/`:13888`) — so the second send of the `plain200` row takes at
/// least 100 ms and the same send of the `zero` row does not. `{send_timeout_close, true}` turns
/// the *next* send into the connection ladder's `enotconn` and leaves the descriptor gone
/// (`sockname` `ebadf`).
#[test]
fn the_oracle_reports_the_measured_send_timeout_rows() {
    let Some(rows) = oracle_rows() else {
        return;
    };
    let row = |label: &str| {
        rows.iter()
            .find(|(name, _)| name == label)
            .map(|(_, row)| row)
            .unwrap_or_else(|| panic!("the probe reports the `{label}` row"))
    };

    let plain = row("plain200");
    assert_eq!(
        plain.results, "[ok,{error,timeout},{error,timeout}]",
        "a fired stall answers `{{error, timeout}}` and the queued remainder is never answered \
         again"
    );
    assert_eq!(plain.getopts, "[200,false]", "both options read back");
    assert_eq!(plain.peer, "ok", "the socket survives the timeout");
    assert_eq!(plain.sock, "ok", "and still owns its descriptor");
    assert!(
        plain.ms[1] >= 100,
        "the 200 ms timer really waited before the second send answered: {:?}",
        plain.ms
    );
    assert!(
        plain.ms[2] >= 100,
        "and so did the third: {:?}",
        plain.ms
    );

    let close = row("close200");
    assert_eq!(
        close.results, "[ok,{error,timeout},{error,enotconn}]",
        "`{{send_timeout_close, true}}` ends the connection, so the third send is the ladder's \
         `enotconn`"
    );
    assert_eq!(close.getopts, "[200,true]", "both options read back");
    assert_eq!(
        close.peer, "{error,enotconn}",
        "the descriptor is no longer connected"
    );
    assert_eq!(
        close.sock, "{error,ebadf}",
        "and its descriptor is closed"
    );
    assert!(
        close.ms[1] >= 100,
        "the timeout still waits: {:?}",
        close.ms
    );

    let zero = row("zero");
    assert_eq!(
        zero.results, "[ok,{error,timeout},{error,timeout}]",
        "`{{send_timeout, 0}}` answers each stalled send `{{error, timeout}}`"
    );
    assert_eq!(zero.getopts, "[0,false]", "the zero timeout reads back");
    assert_eq!(zero.peer, "ok", "and the socket survives it");
    assert!(
        zero.ms[1] < 100,
        "`{{send_timeout, 0}}` answers at once instead of arming a timer: {:?}",
        zero.ms
    );
    assert!(
        plain.ms[1] >= zero.ms[1] + 100,
        "the relationship the 200 ms option stands for: `plain200` waits, `zero` does not \
         (plain {:?}, zero {:?})",
        plain.ms,
        zero.ms
    );
}
