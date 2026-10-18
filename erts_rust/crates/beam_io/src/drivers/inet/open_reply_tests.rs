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
//! The `INET_REQ_OPEN`(1) reply on a port that is **not** closed — the C's `exbadseq` ladder.
//!
//! `inet_ctl_open` (`inet_drv.c:5118`-`:5132`) opens with
//! `if (desc->state != INET_STATE_CLOSED) return ctl_xerror(EXBADSEQ, rbuf, rsize);`, so a second
//! `open` is a sequence error and not the generic `einval` an argument fault would answer. The
//! rows here are cross-checked byte-for-byte against the system `erl`; see [`open_reply_rows`].
//!
//! [`open_reply_rows`]: self::the_listening_and_connected_rows_agree_with_the_erl_oracle
use super::opcode_tests::{
    assert_agrees, bare_port, connection, erl_stdout, loopback_arg, op, open_arg, opened_port,
    oracle_expression, oracle_replies, Call, Rig, Rule, CONTROL_MAGIC,
};
use super::opcode_tests_udp::{bare_port as udp_bare_port, open_arg as udp_open_arg};
use super::wire::{ctl_ok, ctl_xerror};
use super::{InetState, INET_INFINITY};
/// The C request numbers this suite drives (`inet_drv.c`'s `INET_REQ_*` enum).
const OPEN: u32 = 1; // INET_REQ_OPEN
const CLOSE: u32 = 2; // INET_REQ_CLOSE
const CONNECT: u32 = 3; // INET_REQ_CONNECT
const BIND: u32 = 6; // INET_REQ_BIND
const GETFD: u32 = 14; // INET_REQ_GETFD
const GETSTATUS: u32 = 16; // INET_REQ_GETSTATUS
const LISTEN: u32 = 27; // INET_REQ_LISTEN
/// The C's `ctl_xerror(EXBADSEQ)` reply — the result tag `INET_REP_ERROR` (0) followed by the
/// error **name**, `inet_drv.c:1630`'s `#define EXBADSEQ "exbadseq"`.
///
/// Measured on OTP 29.1.1 / erts-17.1 with a raw `tcp_inet` port:
/// `port_control(P, 1+magic, <<1,1,0>>)` opens (`[1]`) and the same call again answers exactly
/// these nine bytes.
const EXBADSEQ_REPLY: [u8; 9] = [0, 101, 120, 98, 97, 100, 115, 101, 113];
/// The `INET_REQ_CONNECT` operand: the C's `Timeout(4)` (here `INET_INFINITY`, i.e. `-1`) followed
/// by the packed `127.0.0.1:port` address (`inet_drv.c:12064`).
fn connect_arg(port: u16) -> Vec<u8> {
    let mut arg = INET_INFINITY.to_be_bytes().to_vec();
    arg.extend_from_slice(&loopback_arg(port));
    arg
}
/// `INET_REQ_GETSTATUS`(16)'s state code (`inet_drv.c:11128`).
fn status(port: &mut Rig) -> u32 {
    let reply = port.control(&op(GETSTATUS), &[]);
    assert_eq!(reply.len(), 5, "GETSTATUS is [1] ++ state_be32: {reply:?}");
    assert_eq!(reply[0], 1, "the success tag");
    u32::from_be_bytes([reply[1], reply[2], reply[3], reply[4]])
}
// -----------------------------------------------------------------------
// The rows, on the Rust side
// -----------------------------------------------------------------------
/// **A fresh port opens, and every later `open` is the C's sequence error.**
///
/// `inet_ctl_open` (`inet_drv.c:5118`-`:5132`) guards on `state != INET_STATE_CLOSED` and answers
/// `[0] ++ "exbadseq"`; the guard comes **after** the request's operand decoding
/// (`tcp_inet_ctl`, `:11968`-`:11995`), which is why a malformed operand still answers its own
/// refusal (pinned by `opcode_tests_states`'s open rows) and why this suite only drives
/// well-formed operands.
///
/// Both wire forms are the same body: the decimal `Op` plus [`CONTROL_MAGIC`] that `prim_inet`
/// sends, and the named `"open"` verb. The descriptor must also survive — the refusal is not
/// allowed to replace (or leak) the live socket.
#[test]
fn a_fresh_port_opens_and_the_second_open_is_the_c_sequence_error() {
    assert_eq!(
        ctl_xerror("exbadseq"),
        EXBADSEQ_REPLY.to_vec(),
        "`ctl_xerror`'s spelling of the C's `EXBADSEQ`"
    );
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(OPEN), &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN on a port in INET_STATE_CLOSED creates the socket"
    );
    let fd = port.control(&op(GETFD), &[]);
    assert_eq!(fd.len(), 5, "GETFD is [1] ++ fd_be32: {fd:?}");
    let decimal = (u64::from(OPEN) + CONTROL_MAGIC).to_string();
    assert_eq!(decimal, op(OPEN), "the numeric route's wire form");
    assert_eq!(
        port.control(&decimal, &open_arg()),
        ctl_xerror("exbadseq"),
        "a second INET_REQ_OPEN on the live socket"
    );
    assert_eq!(
        port.control("open", &open_arg()),
        ctl_xerror("exbadseq"),
        "the named `open` verb answers identically"
    );
    assert_eq!(
        port.control(&op(GETFD), &[]),
        fd,
        "so a refused `open` neither replaces nor leaks the descriptor"
    );
}
/// **A listening port answers `exbadseq` too** — the guard is on `state != CLOSED`, not on "has a
/// socket", and `INET_STATE_LISTENING` is one of the non-closed states (`inet_drv.c:5130`).
#[test]
fn a_listening_port_answers_exbadseq_to_a_second_open() {
    let mut port = opened_port();
    let bound = port.control(&op(BIND), &loopback_arg(0));
    assert_eq!(bound[0], 1, "INET_REQ_BIND: {bound:?}");
    assert_eq!(port.control(&op(LISTEN), &[0, 5]), ctl_ok(), "INET_REQ_LISTEN");
    assert_eq!(
        status(&mut port),
        InetState::Listening.state_code(),
        "the port is listening"
    );
    assert_eq!(
        port.control(&op(OPEN), &open_arg()),
        ctl_xerror("exbadseq"),
        "a second `open` on a listener"
    );
    assert_eq!(
        port.control("open", &open_arg()),
        ctl_xerror("exbadseq"),
        "on the named route as well"
    );
    assert_eq!(
        status(&mut port),
        InetState::Listening.state_code(),
        "and the refusal left the listener untouched"
    );
}
/// **A connected port answers `exbadseq`** — the third non-closed state, over a real loopback
/// connection (`INET_STATE_CONNECTED`, so a socket and a peer both exist).
#[test]
fn a_connected_port_answers_exbadseq_to_a_second_open() {
    let mut conn = connection();
    assert_eq!(
        status(&mut conn.port),
        InetState::Connected.state_code(),
        "the adopted socket has a peer"
    );
    assert_eq!(
        conn.port.control(&op(OPEN), &open_arg()),
        ctl_xerror("exbadseq"),
        "a second `open` on a connected port"
    );
    assert_eq!(
        conn.port.control("open", &open_arg()),
        ctl_xerror("exbadseq"),
        "on the named route as well"
    );
    assert_eq!(
        status(&mut conn.port),
        InetState::Connected.state_code(),
        "and the connection is still up"
    );
}
/// **The datagram driver answers the same** — `packet_inet_ctl`'s `INET_REQ_OPEN` arm
/// (`inet_drv.c:14577`-`:14620`) calls the very same `inet_ctl_open`, so the sequence error and
/// its position after the operand decoding are shared with the stream driver.
#[test]
fn the_datagram_driver_answers_exbadseq_to_a_second_open() {
    let mut port = udp_bare_port();
    assert_eq!(
        port.control(&op(OPEN), &udp_open_arg()),
        ctl_ok(),
        "the datagram socket is created"
    );
    let fd = port.control(&op(GETFD), &[]);
    assert_eq!(
        port.control(&op(OPEN), &udp_open_arg()),
        ctl_xerror("exbadseq"),
        "a second INET_REQ_OPEN on a live datagram port"
    );
    assert_eq!(
        port.control("open", &udp_open_arg()),
        ctl_xerror("exbadseq"),
        "and on the named route"
    );
    assert_eq!(
        port.control(&op(GETFD), &[]),
        fd,
        "the refused `open` kept the descriptor"
    );
}
/// **A closed port opens again** — the row that proves the guard did not become "never reopen".
///
/// `INET_REQ_CLOSE` runs `erl_inet_close` (`inet_drv.c:5102`-`:5117`), whose owned arm closes the
/// descriptor and moves the state to `INET_STATE_CLOSED`, so `inet_ctl_open`'s guard
/// (`:5130`) passes again and only a *second* open after the reopen is refused.
#[test]
fn a_closed_port_opens_again() {
    let mut port = opened_port();
    assert_eq!(
        port.control(&op(OPEN), &open_arg()),
        ctl_xerror("exbadseq"),
        "still live, so still refused"
    );
    assert_eq!(port.control(&op(CLOSE), &[]), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(
        status(&mut port),
        InetState::Closed.state_code(),
        "the owned close returns the state to INET_STATE_CLOSED"
    );
    assert_eq!(
        port.control(&op(OPEN), &open_arg()),
        ctl_ok(),
        "so a closed port opens again"
    );
    let reopened = port.control(&op(GETFD), &[]);
    assert_eq!(reopened[0], 1, "with a real descriptor: {reopened:?}");
    assert_eq!(reopened.len(), 5, "GETFD is [1] ++ fd_be32");
    assert_eq!(
        status(&mut port),
        InetState::Open.state_code(),
        "the reopened socket is a fresh unconnected one (the descriptor number itself is the \
         kernel's to reuse)"
    );
    assert_eq!(
        port.control(&op(OPEN), &open_arg()),
        ctl_xerror("exbadseq"),
        "and the reopened port is a non-closed state again"
    );
}
/// The same close/reopen rule on the datagram driver: a `close` is `erl_inet_close` there too, so
/// `open` is legal again and only the second `open` after it is refused.
#[test]
fn a_closed_datagram_port_opens_again() {
    let mut port = udp_bare_port();
    assert_eq!(port.control(&op(OPEN), &udp_open_arg()), ctl_ok());
    assert_eq!(
        port.control(&op(OPEN), &udp_open_arg()),
        ctl_xerror("exbadseq")
    );
    assert_eq!(port.control(&op(CLOSE), &[]), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(
        port.control(&op(OPEN), &udp_open_arg()),
        ctl_ok(),
        "a closed datagram port opens again"
    );
    assert_eq!(
        port.control(&op(OPEN), &udp_open_arg()),
        ctl_xerror("exbadseq"),
        "and refuses the next one"
    );
}
// -----------------------------------------------------------------------
// The oracle cross-checks
// -----------------------------------------------------------------------
/// The `tcp_inet` open/close/reopen sequence, byte-for-byte against the system driver:
/// `open` ⇒ `[1]`, `open` ⇒ `[0]exbadseq`, `close` ⇒ `[1]`, `open` ⇒ `[1]`, `open` ⇒
/// `[0]exbadseq`.
///
/// **Skips loudly** when `erl` is not on `PATH` or does not answer within the harness budget.
#[test]
fn the_tcp_open_rows_agree_with_the_erl_oracle() {
    let calls = [
        Call::req(OPEN, open_arg().to_vec(), Rule::Exact),
        Call::req(OPEN, open_arg().to_vec(), Rule::Exact),
        Call::req(CLOSE, Vec::<u8>::new(), Rule::Exact),
        Call::req(OPEN, open_arg().to_vec(), Rule::Exact),
        Call::req(OPEN, open_arg().to_vec(), Rule::Exact),
    ];
    let Some(dump) = erl_stdout(&oracle_expression("tcp_inet", &calls)) else {
        eprintln!("SKIP: no `erl` oracle for the tcp_inet open rows");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), calls.len(), "one reply per call: {dump}");
    let mut port = bare_port();
    let rust = port.run(&calls);
    for (index, call) in calls.iter().enumerate() {
        assert_agrees(
            &format!("tcp_inet open row #{index} ({} of {:?})", call.value, call.arg),
            call.rule,
            &oracle[index],
            &rust[index],
        );
    }
    assert_eq!(oracle[0], ctl_ok(), "the oracle's first open: {dump}");
    assert_eq!(
        oracle[1],
        EXBADSEQ_REPLY.to_vec(),
        "the oracle's second open is `[0]` ++ \"exbadseq\": {dump}"
    );
    assert_eq!(oracle[2], ctl_ok(), "the oracle's close: {dump}");
    assert_eq!(oracle[3], ctl_ok(), "the oracle's reopen: {dump}");
    assert_eq!(
        oracle[4],
        EXBADSEQ_REPLY.to_vec(),
        "and its second open after the reopen: {dump}"
    );
}
/// The datagram half of [`the_tcp_open_rows_agree_with_the_erl_oracle`], over the same
/// open/close/reopen sequence on a raw `udp_inet` port.
#[test]
fn the_udp_open_rows_agree_with_the_erl_oracle() {
    let calls = [
        Call::req(OPEN, udp_open_arg().to_vec(), Rule::Exact),
        Call::req(OPEN, udp_open_arg().to_vec(), Rule::Exact),
        Call::req(CLOSE, Vec::<u8>::new(), Rule::Exact),
        Call::req(OPEN, udp_open_arg().to_vec(), Rule::Exact),
        Call::req(OPEN, udp_open_arg().to_vec(), Rule::Exact),
    ];
    let Some(dump) = erl_stdout(&oracle_expression("udp_inet", &calls)) else {
        eprintln!("SKIP: no `erl` oracle for the udp_inet open rows");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), calls.len(), "one reply per call: {dump}");
    let mut port = udp_bare_port();
    let rust = port.run(&calls);
    for (index, call) in calls.iter().enumerate() {
        assert_agrees(
            &format!("udp_inet open row #{index} ({} of {:?})", call.value, call.arg),
            call.rule,
            &oracle[index],
            &rust[index],
        );
    }
    assert_eq!(
        oracle[1],
        EXBADSEQ_REPLY.to_vec(),
        "the oracle's datagram sequence error: {dump}"
    );
}
/// **The listener and the connected socket, measured on the system driver.**
///
/// The oracle expression drives two raw `tcp_inet` ports in one `erl` run: the first is opened,
/// bound and listened on before its second `open`; the second is opened and a raw
/// `INET_REQ_CONNECT`(3) is issued against a real `gen_tcp` listener before its second `open`.
/// Only the four `open` replies are printed — the C's connect reply carries an async-id tail this
/// floor answers synchronously, which `opcode_tests_states` owns.
///
/// **Skips loudly** when `erl` is not on `PATH` or does not answer within the harness budget.
#[test]
fn the_listening_and_connected_rows_agree_with_the_erl_oracle() {
    let expression = format!(
        "J = fun(R) -> io:format(\"~w|\", [R]) end, A = <<1,1,0>>, \
         P = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         J(erlang:port_control(P, {open}, A)), \
         erlang:port_control(P, {bind}, <<4,0,0>>), \
         erlang:port_control(P, {listen}, <<0,5>>), \
         J(erlang:port_control(P, {open}, A)), \
         erlang:port_close(P), \
         {{ok, LS}} = gen_tcp:listen(0, [{{ip,{{127,0,0,1}}}}]), \
         {{ok, LP}} = inet:port(LS), \
         Q = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         J(erlang:port_control(Q, {open}, A)), \
         erlang:port_control(Q, {connect}, <<255,255,255,255, 4, LP:16>>), \
         J(erlang:port_control(Q, {open}, A)), \
         erlang:port_close(Q), gen_tcp:close(LS)",
        open = op(OPEN),
        bind = op(BIND),
        listen = op(LISTEN),
        connect = op(CONNECT),
    );
    let Some(dump) = erl_stdout(&expression) else {
        eprintln!("SKIP: no `erl` oracle for the listening/connected open rows");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 4, "four printed rows: {dump}");
    // The listener half, at the same state points on one port.
    let mut listening = bare_port();
    assert_eq!(
        listening.control(&op(OPEN), &open_arg()),
        oracle[0],
        "the first INET_REQ_OPEN on the port that becomes a listener"
    );
    let bound = listening.control(&op(BIND), &loopback_arg(0));
    assert_eq!(bound[0], 1, "INET_REQ_BIND: {bound:?}");
    assert_eq!(listening.control(&op(LISTEN), &[0, 5]), ctl_ok(), "INET_REQ_LISTEN");
    assert_eq!(
        status(&mut listening),
        InetState::Listening.state_code(),
        "the port is listening"
    );
    assert_equation(&mut listening, &oracle[1], "a second `open` on a listening port");
    // The connected half.
    let peer = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let peer_port = peer.local_addr().expect("an address").port();
    let mut connecting = bare_port();
    assert_eq!(
        connecting.control(&op(OPEN), &open_arg()),
        oracle[2],
        "the first INET_REQ_OPEN on the port that becomes connected"
    );
    assert_eq!(
        connecting.control(&op(CONNECT), &connect_arg(peer_port)),
        ctl_ok(),
        "the synchronous connect completion answers the bare [1]"
    );
    assert_eq!(
        status(&mut connecting),
        InetState::Connected.state_code(),
        "the socket now has a peer"
    );
    assert_equation(&mut connecting, &oracle[3], "a second `open` on a connected port");
}
/// Asserts one `open` reply against the oracle's row, on both wire forms.
fn assert_equation(port: &mut Rig, oracle: &[u8], label: &str) {
    assert_eq!(
        port.control(&op(OPEN), &open_arg()),
        oracle,
        "{label}: the numeric route must answer the oracle's bytes"
    );
    assert_eq!(
        port.control("open", &open_arg()),
        oracle,
        "{label}: and the named route must answer the same bytes"
    );
}
