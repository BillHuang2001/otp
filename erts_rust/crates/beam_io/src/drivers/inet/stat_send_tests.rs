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

//! Oracle-cross-checked `INET_REQ_GETSTAT` cover for the `tcp_inet` **send** counters.
//!
//! A sibling `#[path]` test module of `stat.rs` so the source half stays small. Every case is
//! measured against the system `erl` (OTP 29.1.1 / erts-17.1) and skips **loudly** when it is
//! absent. `stat.rs` is the arithmetic; this file proves the drivers reach it at the right
//! counting site, with the right length, once per command.
//!
//! Two C sites produce every figure asserted here:
//!
//! * `inet_output_count(desc, len + h_len)` (`erts/emulator/drivers/common/inet_drv.c:11560`-
//!   `:11582`) — the accounting, whose `send_cnt`/`send_max`/`send_avg`/`send_oct` update the
//!   [`super::stat::InetStat::output`] half reproduces line for line;
//! * `inet_fill_stat` (`:10732`-`:10792`) — the reply the runtime reads, whose 5-byte (ops 1-8)
//!   and 9-byte (ops 9, 10) records and request order
//!   [`super::super::wire::getstat_reply`] reproduces.
//!
//! The runtime has **two** write routes and each has its own counting site, so every sequence
//! below runs over both:
//!
//! * the named `send` control verb — [`super::super::TcpInetDriver::verb_send`], counting
//!   `arg.len() + header_len` at `inet_drv.c:13763` (`tcp_send`);
//! * `erlang:port_command/2` — `tcp_inet_commandv` → `write_command`, counting
//!   `payload_len + h_len` at `inet_drv.c:13879` (`tcp_sendv`) **once per command**, after the
//!   `prim_inet` caller tag has been stripped and after the `!IS_CONNECTED` early return.
use std::io::Write;

use super::super::opcode_tests::{bare_port as bare_tcp_port, erl_stdout, op, Rig};
use super::super::stat_tests::{stat32, stat64, TcpPair};
use super::super::wire::ctl_ok;
use super::super::{INET_LOPT_PACKET, INET_REP_OK};
use beam_common::BeamError;

/// The `INET_STAT_*` op codes (`inet_drv.c:961`-`:970`), in the C's own order.
const RECV_CNT: u8 = 1;
const RECV_MAX: u8 = 2;
const RECV_AVG: u8 = 3;
const RECV_DVI: u8 = 4;
const SEND_CNT: u8 = 5;
const SEND_MAX: u8 = 6;
const SEND_AVG: u8 = 7;
const SEND_PEND: u8 = 8;
const RECV_OCT: u8 = 9;
const SEND_OCT: u8 = 10;

/// Every counter, in op order — the request that renders the whole 59-byte block.
const ALL_COUNTERS: [u8; 10] = [
    RECV_CNT, RECV_MAX, RECV_AVG, RECV_DVI, SEND_CNT, SEND_MAX, SEND_AVG, SEND_PEND, RECV_OCT,
    SEND_OCT,
];

/// The five counters the send half owns, in op order.
const SEND_COUNTERS: [u8; 5] = [SEND_CNT, SEND_MAX, SEND_AVG, SEND_PEND, SEND_OCT];

/// The **internal** `{packet, …}` codes (`enum PacketParseType`, `packet_parser.h`): `prim_inet`
/// translates the user's `{packet, 4}` into `TCP_PB_4 = 3` before the option reaches the driver
/// (`inet_drv.c:896`-`:904`). Only the three this file needs.
const TCP_PB_RAW: u32 = 0;
const TCP_PB_2: u32 = 2;
const TCP_PB_4: u32 = 3;

/// The exact bytes `inet_fill_stat` (`inet_drv.c:10732`-`:10792`) renders for `ops` carrying
/// `values`, in request order: the `[1]` tag, then a five-byte `[op, value_be32]` record per
/// 32-bit counter and a nine-byte `[op, value_be64]` one for the two octet counters.
fn records(ops: &[u8], values: &[u64]) -> Vec<u8> {
    assert_eq!(ops.len(), values.len(), "one value per requested op");
    let mut reply = vec![INET_REP_OK];
    for (&code, &value) in ops.iter().zip(values) {
        reply.push(code);
        if code == RECV_OCT || code == SEND_OCT {
            reply.extend_from_slice(&value.to_be_bytes());
        } else {
            // The C's `put_int32` copies the low 32 bits (`sys.h:1286`), so a 32-bit record
            // truncates rather than saturating.
            reply.extend_from_slice(&(value as u32).to_be_bytes());
        }
    }
    reply
}

/// Sets `{packet, Code}` through the numeric `setopts`(7) verb, with the internal `TCP_PB_*` code
/// (option [`INET_LOPT_PACKET`] 23, `inet_drv.c:898`).
fn set_packet(port: &mut Rig, code: u32) {
    let mut record = vec![INET_LOPT_PACKET];
    record.extend_from_slice(&code.to_be_bytes());
    assert_eq!(
        port.control(&op(7), &record),
        ctl_ok(),
        "INET_REQ_SETOPTS(7): the `packet` option"
    );
}

/// Which of the runtime's two write routes a command takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    /// The named `send` control verb — [`super::super::TcpInetDriver::verb_send`], `arg.len() +
    /// header_len` at `inet_drv.c:13763`.
    Named,
    /// `erlang:port_command/2` → `write_command`, `payload_len + h_len` at `inet_drv.c:13879`.
    Command,
}

impl Route {
    fn label(self) -> &'static str {
        match self {
            Route::Named => "the named `send` verb",
            Route::Command => "`erlang:port_command/2`",
        }
    }
}

/// Queues `payload` on `route` and lets it go out on the wire.
///
/// The `port_command` form is the one `prim_inet:send/3` builds — a big-endian 16-bit tag length,
/// the tag, then the caller's data (`prim_inet.erl:584`) — with an **empty** tag (`[0,0]`): the
/// two length-word bytes are stripped before the counting site reads the payload, so what is
/// counted is exactly `payload.len() + h_len`, and an empty tag stages no `{inet_reply, …}`.
fn send_on(route: Route, port: &mut Rig, payload: &[u8]) {
    match route {
        Route::Named => assert_eq!(
            port.control("send", payload),
            ctl_ok(),
            "the `send` verb answers `[1]`"
        ),
        Route::Command => {
            let mut command = vec![0u8, 0u8];
            command.extend_from_slice(payload);
            port.command(&command)
                .expect("`erlang:port_command/2` answers `true`");
        }
    }
}

/// The `[V || {_,V} <- Stat]` rows an oracle probe prints with `io:format("~w|", …)`, parsed back
/// into numbers — one row per `|`-separated list.
///
/// The multi-row form of [`super::super::stat_tests`]'s single-row `oracle_counters`: every case
/// below reads the counters at several points of one TCP conversation, and booting `erl` once per
/// step would only make the suite slower. `None` — after the probe helper's own loud `SKIP:`
/// banner — when `erl` is absent or does not answer in time; a non-empty but unparsable dump is a
/// bug in the probe and fails the test.
fn oracle_rows(expression: &str) -> Option<Vec<Vec<u32>>> {
    let Some(dump) = erl_stdout(expression) else {
        return None;
    };
    let mut rows = Vec::new();
    for field in dump.split('|') {
        if !field.trim_start().starts_with('[') {
            continue;
        }
        let inner = field
            .split_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list)
            .unwrap_or_else(|| panic!("no list in the oracle's row {field:?}"));
        rows.push(
            inner
                .split(',')
                .map(|number| {
                    number
                        .trim()
                        .parse::<u32>()
                        .unwrap_or_else(|_| panic!("an unparsable number in {dump:?}"))
                })
                .collect(),
        );
    }
    assert!(!rows.is_empty(), "the oracle printed no row: {dump:?}");
    Some(rows)
}

// -----------------------------------------------------------------------
// Case A — `{packet, raw}`: five commands, one of them zero-length
// -----------------------------------------------------------------------

/// The `{packet, raw}` sequence: fresh, a 5-byte send, a **zero-length** send, a 10-byte send and
/// two more zero-length sends, read as `[recv_cnt, recv_max, recv_avg, recv_dvi, send_cnt,
/// send_max, send_avg, send_pend, recv_oct, send_oct]`.
///
/// Measured on OTP 29.1.1 / erts-17.1 (the expression [`the_raw_send_sequence_agrees_with_the_oracle`]
/// runs).
const RAW_ROWS: [[u64; 10]; 6] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 1, 5, 5, 0, 0, 5],
    [0, 0, 0, 0, 2, 5, 2, 0, 0, 5],
    [0, 0, 0, 0, 3, 10, 5, 0, 0, 15],
    [0, 0, 0, 0, 4, 10, 3, 0, 0, 15],
    [0, 0, 0, 0, 5, 10, 3, 0, 0, 15],
];

/// The payload length of each `RAW_ROWS` step (the fresh row is the state *before* the first one).
const RAW_SENDS: [usize; 5] = [5, 0, 10, 0, 0];

/// Every command advances `send_cnt` by exactly one — zero-length ones included — and adds
/// `payload + h_len` octets (`inet_output_count`, `inet_drv.c:11560`-`:11582`), on **both** write
/// routes.
///
/// The zero-length step is the discriminator for the running mean: `send_avg` is a `f64` in the
/// descriptor and the C promotes `len` to `double` before dividing (`desc->send_avg += (len -
/// avg)/n`, `:11567` and `:11578` — `inet_output_count` and its `inet_input_count` twin), so
/// after `5, 0` the mean is `5 + (0 - 5)/2 = 2.5` and `inet_fill_stat`'s `(unsigned long)
/// desc->send_avg` renders **2**. An integer division would give `5 + (0-5)/2 = 3` here and `3`
/// at the third step, where the oracle answers `2` — measured, and pinned below.
#[test]
fn the_raw_send_sequence_counts_every_command_including_zero_length_ones() {
    for route in [Route::Named, Route::Command] {
        let mut pair = TcpPair::new();
        // `{packet, raw}` (the descriptor's default, set explicitly so the row cannot depend on
        // it): no header is written, so `h_len` is `0` and a command counts its payload alone.
        set_packet(&mut pair.port, TCP_PB_RAW);
        assert_eq!(
            pair.port.control(&op(11), &ALL_COUNTERS),
            records(&ALL_COUNTERS, &RAW_ROWS[0]),
            "{}: a fresh connection has counted nothing",
            route.label()
        );

        for (step, &len) in RAW_SENDS.iter().enumerate() {
            send_on(route, &mut pair.port, &vec![b'x'; len]);
            let expected = &RAW_ROWS[step + 1];
            assert_eq!(
                pair.port.control(&op(11), &ALL_COUNTERS),
                records(&ALL_COUNTERS, expected),
                "{}: after step {} ({} octet(s)); cnt, max, avg, pend, oct = {:?}",
                route.label(),
                step,
                len,
                expected
            );
        }
        assert_eq!(
            stat32(&mut pair.port, SEND_CNT),
            5,
            "{}: five commands, five countings",
            route.label()
        );
        assert_eq!(
            stat64(&mut pair.port, SEND_OCT),
            15,
            "{}: 5 + 0 + 10 octets",
            route.label()
        );
    }
}

/// The same five-command sequence on the live oracle, row for row.
///
/// `gen_tcp:send/2` goes through `prim_inet:send/3` → `erlang:port_command/2`, i.e. the
/// `Route::Command` counting site; the rows are the ones both of this floor's routes must
/// reproduce (the named verb is this floor's own entry point into the same
/// `inet_output_count`).
#[test]
fn the_raw_send_sequence_agrees_with_the_oracle() {
    let Some(oracle) = oracle_rows(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,0},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,0}]), \
         {ok,S}=gen_tcp:accept(L), \
         G=fun()->{ok,St}=inet:getstat(S,[recv_cnt,recv_max,recv_avg,recv_dvi,send_cnt, \
         send_max,send_avg,send_pend,recv_oct,send_oct]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]) end, \
         G(), ok=gen_tcp:send(S,<<\"12345\">>), G(), \
         ok=gen_tcp:send(S,<<>>), G(), \
         ok=gen_tcp:send(S,<<\"0123456789\">>), G(), \
         ok=gen_tcp:send(S,<<>>), G(), \
         ok=gen_tcp:send(S,<<>>), G(), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    let expected: Vec<Vec<u32>> = RAW_ROWS
        .iter()
        .map(|row| row.iter().map(|&value| value as u32).collect())
        .collect();
    assert_eq!(oracle, expected, "the oracle's raw send rows");
}

// -----------------------------------------------------------------------
// Case B — `{packet, 2}`: the framing header is counted too
// -----------------------------------------------------------------------

/// The `{packet, 2}` sequence, read as `[send_cnt, send_max, send_avg, send_pend, send_oct]`:
/// fresh, a zero-length send, a 5-byte send and another zero-length send.
///
/// Measured on OTP 29.1.1 / erts-17.1 (the expression
/// [`the_framed_send_sequence_agrees_with_the_oracle`] runs).
const FRAMED_ROWS: [[u64; 5]; 4] = [
    [0, 0, 0, 0, 0],
    [1, 2, 2, 0, 2],
    [2, 7, 4, 0, 9],
    [3, 7, 3, 0, 11],
];

/// The payload length of each `FRAMED_ROWS` step.
const FRAMED_SENDS: [usize; 3] = [0, 5, 0];

#[test]
fn the_framed_send_sequence_counts_the_header_on_every_command() {
    for route in [Route::Named, Route::Command] {
        let mut pair = TcpPair::new();
        set_packet(&mut pair.port, TCP_PB_2);
        assert_eq!(
            pair.port.control(&op(11), &SEND_COUNTERS),
            records(&SEND_COUNTERS, &FRAMED_ROWS[0]),
            "{}: nothing counted yet",
            route.label()
        );

        for (step, &len) in FRAMED_SENDS.iter().enumerate() {
            send_on(route, &mut pair.port, &vec![b'y'; len]);
            let expected = &FRAMED_ROWS[step + 1];
            assert_eq!(
                pair.port.control(&op(11), &SEND_COUNTERS),
                records(&SEND_COUNTERS, expected),
                "{}: after step {} ({} octet(s)); cnt, max, avg, pend, oct = {:?}",
                route.label(),
                step,
                len,
                expected
            );
        }
    }
}

/// The oracle's `{packet, 2}` rows. `send_oct` counts the two-byte header of every command, the
/// zero-length ones included (`inet_output_count(len + h_len)`, `inet_drv.c:13763`/`:13879`), and
/// the mean follows the same `f64` arithmetic (`:11567`/`:11578`): after `0+2` then `5+2` it is
/// `2 + (7 - 2)/2 = 4.5` → rendered `4`, and after a further `0+2` it is
/// `4.5 + (2 - 4.5)/3 = 3.666…` → rendered `3`.
#[test]
fn the_framed_send_sequence_agrees_with_the_oracle() {
    let Some(oracle) = oracle_rows(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,2},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,2}]), \
         {ok,S}=gen_tcp:accept(L), \
         G=fun()->{ok,St}=inet:getstat(S,[send_cnt,send_max,send_avg,send_pend,send_oct]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]) end, \
         G(), ok=gen_tcp:send(S,<<>>), G(), \
         ok=gen_tcp:send(S,<<\"hello\">>), G(), \
         ok=gen_tcp:send(S,<<>>), G(), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    let expected: Vec<Vec<u32>> = FRAMED_ROWS
        .iter()
        .map(|row| row.iter().map(|&value| value as u32).collect())
        .collect();
    assert_eq!(oracle, expected, "the oracle's framed send rows");
}

// -----------------------------------------------------------------------
// Case D — the whole 59-byte reply after a mixed `{packet, 4}` sequence
// -----------------------------------------------------------------------

/// The `{packet, 4}` mixed sequence: send `"hello"` (5 payload + 4 header = 9 octets), send an
/// empty body (4 header octets), then receive one peer-sent frame carrying `"xy"` (6 octets,
/// header included) — and read the **whole** counter block.
///
/// The 59 bytes are compared as bytes, not as parsed numbers, so the record widths
/// (`inet_fill_stat`'s `put_int32` for ops 1-8 and its 64-bit writes for ops 9/10, `:10732`-
/// `:10792`) and the request order are pinned together with the values.
///
/// Measured on OTP 29.1.1 / erts-17.1 for `erlang:port_control(S, 11+16#03f1a300,
/// <<1,2,3,4,5,6,7,8,9,10>>)`; the parsed counters are `[1,6,6,0, 2,9,6,0, 6,13]`.
#[test]
fn the_whole_counter_block_renders_after_a_mixed_framed_sequence() {
    let mut pair = TcpPair::new();
    set_packet(&mut pair.port, TCP_PB_4);

    // Two commands out (`"hello"` and an empty body), then one framed message in.
    send_on(Route::Command, &mut pair.port, b"hello");
    send_on(Route::Command, &mut pair.port, b"");
    pair.peer
        .write_all(&[0, 0, 0, 2, b'x', b'y'])
        .expect("the peer writes one `{packet,4}` frame");
    pair.recv_until(2);
    assert_eq!(
        pair.port.take_input(2),
        b"xy".to_vec(),
        "the frame's payload, header stripped"
    );

    // Every field, one `INET_REQ_GETSTAT` call at a time.
    let values = [
        u64::from(stat32(&mut pair.port, RECV_CNT)),
        u64::from(stat32(&mut pair.port, RECV_MAX)),
        u64::from(stat32(&mut pair.port, RECV_AVG)),
        u64::from(stat32(&mut pair.port, RECV_DVI)),
        u64::from(stat32(&mut pair.port, SEND_CNT)),
        u64::from(stat32(&mut pair.port, SEND_MAX)),
        u64::from(stat32(&mut pair.port, SEND_AVG)),
        u64::from(stat32(&mut pair.port, SEND_PEND)),
        stat64(&mut pair.port, RECV_OCT),
        stat64(&mut pair.port, SEND_OCT),
    ];
    assert_eq!(
        values, [1, 6, 6, 0, 2, 9, 6, 0, 6, 13],
        "cnt, max, avg, dvi, cnt, max, avg, pend, oct, oct"
    );

    // ... and the same state as one 59-byte block, compared byte for byte.
    let expected: Vec<u8> = vec![
        1, 1, 0, 0, 0, 1, 2, 0, 0, 0, 6, 3, 0, 0, 0, 6, 4, 0, 0, 0, 0, 5, 0, 0, 0, 2, 6, 0, 0, 0,
        9, 7, 0, 0, 0, 6, 8, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 6, 10, 0, 0, 0, 0, 0, 0, 0, 13,
    ];
    assert_eq!(expected.len(), 59, "1 + 8*5 + 2*9");
    assert_eq!(records(&ALL_COUNTERS, &values), expected, "the record layout");
    assert_eq!(
        pair.port.control(&op(11), &ALL_COUNTERS),
        expected,
        "the whole 59-byte block"
    );

    let Some(oracle) = oracle_rows(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,4},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,4}]), \
         {ok,S}=gen_tcp:accept(L), \
         ok=gen_tcp:send(S,<<\"hello\">>), \
         ok=gen_tcp:send(S,<<>>), \
         ok=gen_tcp:send(C,<<\"xy\">>), \
         {ok,<<\"xy\">>}=gen_tcp:recv(S,0,5000), \
         io:format(\"~w|\",[erlang:port_control(S,11+16#03f1a300,<<1,2,3,4,5,6,7,8,9,10>>)]), \
         {ok,St}=inet:getstat(S,[recv_cnt,recv_max,recv_avg,recv_dvi,send_cnt,send_max, \
         send_avg,send_pend,recv_oct,send_oct]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    let [oracle_bytes, oracle_values] = &oracle[..] else {
        panic!("the oracle printed {} row(s), not two", oracle.len());
    };
    assert_eq!(
        *oracle_bytes,
        expected.iter().map(|&byte| u32::from(byte)).collect::<Vec<_>>(),
        "the oracle's 59 raw reply bytes"
    );
    assert_eq!(
        *oracle_values,
        values.iter().map(|&value| value as u32).collect::<Vec<_>>(),
        "and the same state read through `inet:getstat/2`"
    );
}

// -----------------------------------------------------------------------
// Case F — the argument rows (the C's `default: return -1`)
// -----------------------------------------------------------------------

/// An **empty** request appends no record at all: `inet_fill_stat`'s `while (len--)` loop
/// (`inet_drv.c:10740`) never runs, so the reply is the bare `[1]` tag. Measured on the oracle for
/// a bare `open_port({spawn_driver,"tcp_inet"},[])` port.
#[test]
fn an_empty_counter_request_answers_the_bare_tag() {
    let mut port = bare_tcp_port();
    assert_eq!(
        port.control(&op(11), &[]),
        vec![INET_REP_OK],
        "no record, just the tag"
    );
}

/// A counter op outside 1-10 is the C's `default: return -1` (`inet_drv.c:10786`), which
/// `inet_ctl`'s `INET_REQ_GETSTAT` arm propagates as `-1` → `erlang:port_control/3` raises
/// `badarg` — so the **whole** reply is discarded, never truncated at the offending op.
///
/// Measured on the oracle: `port_control(P, 11+16#03f1a300, <<11>>)`, `<<0>>`, `<<255>>` and
/// `<<1,11,2>>` each raise `error:badarg`, the port stays alive and a later `<<5>>` still answers
/// `[1,5,0,0,0,0]`.
#[test]
fn an_unknown_counter_op_discards_the_whole_reply() {
    let mut port = bare_tcp_port();
    for arg in [vec![11u8], vec![0], vec![255], vec![1, 11, 2]] {
        match port.try_control(&op(11), &arg) {
            Err(BeamError::BadArg) => {}
            other => panic!("`getstat` with {arg:?} answered {other:?}, not `badarg`"),
        }
        assert!(
            port.contains(),
            "`badarg` is a raise, not a teardown: the port is still live"
        );
    }
    assert_eq!(
        port.control(&op(11), &[SEND_CNT]),
        records(&[SEND_CNT], &[0]),
        "the port still serves a valid request afterwards"
    );
}

/// The oracle's four refusals and the still-live port, checked in one boot.
///
/// The probe marks each answer `1` for `[1]`, `2` for a raised `badarg` and `0` for anything else,
/// so the expected row is `[1,2,2,2,2,1]`.
#[test]
fn the_unknown_counter_op_refusals_agree_with_the_oracle() {
    let Some(oracle) = oracle_rows(
        "P=open_port({spawn_driver,\"tcp_inet\"},[]), M=16#03f1a300, \
         Out=fun(Arg)->try erlang:port_control(P,11+M,Arg) of [1]->1; _->0 \
         catch error:badarg->2 end end, \
         Alive=case erlang:port_info(P,name) of {name,\"tcp_inet\"}->1; _->0 end, \
         io:format(\"~w|\",[[Out(<<>>),Out(<<11>>),Out(<<0>>),Out(<<255>>),Out(<<1,11,2>>), \
         Alive]]), \
         port_close(P)",
    ) else {
        return;
    };
    assert_eq!(
        oracle,
        vec![vec![1, 2, 2, 2, 2, 1]],
        "`<<>>` answers `[1]`, the four unknown args raise `badarg`, the port survives"
    );
}

// -----------------------------------------------------------------------
// Case G — a short write is counted at issue time, not as the socket takes it
// -----------------------------------------------------------------------

/// A command the kernel cannot take whole is still counted **in full** at issue time, and
/// `send_pend` reports exactly the remainder.
///
/// `inet_output_count(desc, len + h_len)` runs in `tcp_sendv` **before** `sock_send`
/// (`inet_drv.c:13763` for `tcp_send`, `:13879` for the vector form), so `send_oct`/`send_max`
/// carry `16 MiB + 4` — the four-byte `{packet,4}` header included — while the bytes the socket
/// refused sit in the port's `ioq` and are what `inet_fill_stat`'s `driver_sizeq(desc->port)`
/// (`:10766`) renders as `send_pend`. 16 MiB can never fit in the send buffer plus the peer's
/// receive window, so the peer never reading makes the remainder deterministic without any
/// timing budget.
///
/// Measured on the oracle for exactly this traffic: `send_oct = send_max = 16 777 220`,
/// `send_cnt = 1`, `send_pend = 14 188 548` — a strict remainder, never the whole command.
#[test]
fn a_short_write_is_counted_in_full_at_issue_time() {
    const PAYLOAD: usize = 16 * 1024 * 1024;
    let whole = PAYLOAD + 4;

    let mut pair = TcpPair::new();
    set_packet(&mut pair.port, TCP_PB_4);
    send_on(Route::Command, &mut pair.port, &vec![0u8; PAYLOAD]);

    let id = pair.port.id;
    let queued = pair
        .port
        .registry
        .get(id)
        .expect("a live port")
        .output_len();
    assert!(
        queued > 0,
        "the kernel could not take the whole command; a strict remainder must be queued"
    );
    assert!(
        queued < whole,
        "a strict remainder of the command is queued ({queued} of {whole})"
    );

    assert_eq!(
        stat64(&mut pair.port, SEND_OCT),
        whole as u64,
        "the whole command was counted at issue, header included"
    );
    assert_eq!(
        stat32(&mut pair.port, SEND_MAX) as usize,
        whole,
        "and it is the peak, even though the socket took less"
    );
    assert_eq!(
        stat32(&mut pair.port, SEND_CNT),
        1,
        "a partial write is still one command"
    );
    let pend = stat32(&mut pair.port, SEND_PEND) as usize;
    assert_eq!(pend, queued, "`send_pend` is the port's output-queue depth");
    assert!(
        pend > 0 && pend < whole,
        "a strict remainder of the counted command ({pend} of {whole})"
    );
}

/// The oracle's own figures for the same command — the whole length counted, a strict remainder
/// pending — driven through `gen_tcp:send/2` (i.e. the `port_command` route this floor's case
/// above uses).
#[test]
fn the_short_write_counters_agree_with_the_oracle() {
    let Some(oracle) = oracle_rows(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,4},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,4}]), \
         {ok,S}=gen_tcp:accept(L), \
         Big=binary:copy(<<0>>,16*1024*1024), \
         ok=gen_tcp:send(S,Big), \
         {ok,St}=inet:getstat(S,[send_pend,send_oct,send_max,send_cnt]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    let [row] = &oracle[..] else {
        panic!("the oracle printed {} row(s), not one", oracle.len());
    };
    let [oracle_pend, oracle_oct, oracle_max, oracle_cnt] = row[..] else {
        panic!("the oracle answered {} counters, not four", row.len());
    };
    let whole = (16 * 1024 * 1024 + 4) as u32;
    assert_eq!(oracle_oct, whole, "the oracle counted the whole command");
    assert_eq!(oracle_max, whole, "and it is the oracle's peak");
    assert_eq!(oracle_cnt, 1, "one command");
    assert!(oracle_pend > 0, "the oracle's queue kept a remainder");
    assert!(
        oracle_pend < whole,
        "and it is a strict remainder of the command"
    );
}
