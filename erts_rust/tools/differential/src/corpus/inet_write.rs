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
//! Contract **A.4** — the inet **write** data path: what `gen_tcp:send/2` and
//! `gen_udp:send/4` actually put on the wire.
//!
//! [`super::inet_control`] pins the read half of the same drivers (the framed `{packet, 4}` /
//! `{packet, 2}` / `line` parse in all three block modes, the option/address store, the refusal
//! matrices and a passive `gen_udp:recv/3`). This group pins the other direction — the one the
//! *application* drives: the payload a send hands to the driver, the answer the driver hands
//! back, and the bytes the peer's socket really receives. Every row is a two-socket loopback in
//! a single expression: `gen_tcp:listen/2` on port `0`, a `gen_tcp:connect/3` +
//! `gen_tcp:accept/1` pair, the bytes sent on one end and received on the other end's
//! `{active, true}` message queue.
//!
//! ## What each row pins
//!
//! * **`iolist_payload_is_concatenated`** — a send of a *deep* iolist
//!   (`[<<"ab">>, [$c, $d], [<<"e">>, $f], <<"gh">>]`) delivers exactly the 8 bytes the
//!   concatenation names, and the identical flat binary delivers the identical bytes: the
//!   writer flattens nested lists, byte integers and binaries into one stream, and the two
//!   spellings are indistinguishable on the wire.
//! * **`segmented_sends_arrive_as_the_concatenation`** — four successive sends (`<<"ab">>`,
//!   `[<<"cd">>]`, `[<<>>]`, `<<"ef">>`) arrive as the *concatenation* `<<"abcdef">>`: the
//!   stream writer preserves order, and a zero-length send in the middle neither writes nor
//!   disturbs it. The receive side drains by **total length** (a bounded accumulate-until-N
//!   loop), so segment coalescing cannot change the answer.
//! * **`byte_transparent_payload`** — a 256-byte payload carrying every byte value `0…255`
//!   (`list_to_binary(lists:seq(0, 255))`) is delivered back byte-identical: the writer does
//!   no transformation at all, in particular no escaping of `0`, `255` or CR/LF.
//! * **`write_answer_matrix`** — `gen_tcp:send/2`'s three refusal answers against one
//!   listener/connection pair: `{error, enotconn}` on the listening socket (it has no peer),
//!   `{error, closed}` on the connected socket after `gen_tcp:close/1`, and again
//!   `{error, enotconn}`/`{error, closed}` after the listener is closed too.
//! * **`send_after_the_peer_close_notification_is_closed`** — the deterministic peer-close
//!   route: the client waits for its own `{tcp_closed, Cl}` teardown message and *then* sends,
//!   which is `{error, closed}` for a flat binary and for an iolist alike (twice in a row).
//!   Waiting for the notification is what makes the shape deterministic — without it the first
//!   send after the peer's `close` legitimately still answers `ok` (the FIN may not have been
//!   processed yet), which is a race and therefore never pinned.
//! * **`empty_payload_is_a_successful_noop`** — `gen_tcp:send/2` of `<<>>`, of `[]` and of
//!   `[<<>>, []]` all answer `ok`, and the peer receives **only** the following `<<"hi">>`:
//!   a zero-length write is a success and puts no bytes on the stream.
//! * **`write_option_store_and_a_completed_write`** — the write-side option store, observed
//!   through `inet:getopts/2`/`inet:setopts/2` on the connected socket: the
//!   `{send_timeout, infinity}`/`{send_timeout_close, false}`/`{packet_size, 0}`/
//!   `{nodelay, false}`/`{delay_send, false}` defaults, the five stored values echoed back in
//!   request order, the `{send_timeout, -1}`/`{packet_size, -2}` `{error, einval}` refusals, and
//!   the peer's `{recbuf, _}` reduced to a `> 0` integer (its figure is the kernel's, so only
//!   the shape is pinned) — plus one completed write over that socket.
//! * **`large_payload_is_fully_received`** — a 64 KiB payload (`binary:copy/2` of a 16-byte
//!   pattern) written by one `gen_tcp:send/2` is received **in full**: the peer drains until the
//!   byte count is reached and the row pins the total (65536) and a byte sum (the content
//!   checksum of the payload the case itself built). This is the multi-segment case — the
//!   write side has to hand the whole buffer to the driver, not just its first chunk.
//! * **`both_directions_of_the_stream_write`** — the connected pair writes **both** ways: the
//!   client's iolist arrives on the accepted socket and the accepted socket's binary arrives on
//!   the client, so the writer is not tied to the `connect`/`accept` side.
//! * **`line_mode_send_adds_no_prefix`** — `{packet, line}` on the **sending** socket adds
//!   nothing: `<<"hi\n">>` arrives as those three bytes (the EOL included) and `<<"nonl">>`
//!   arrives as those four, with `inet:getopts/2` echoing the mode. `line` is the one block
//!   framing mode whose write side agrees (next section).
//! * **`datagram_write_path`** — the datagram writer: `gen_udp:send/4` of an iolist
//!   (`[<<"ab">>, <<"cd">>]`) is one datagram carrying the 4-byte concatenation, a send of
//!   `<<>>` is a *deliverable zero-length datagram* (the peer's receive answers
//!   `{data, <<>>}`, not a timeout), and an 8192-byte datagram arrives byte-identical.
//!
//! Ids are `inet_write/<snake_case_name>`; the contract is A.4 ([`Contract::DriverAbi`]) plus
//! A.7 ([`Contract::OtpApps`]) where the observation is the `gen_tcp`/`gen_udp`/`inet` module
//! answer rather than a bare port BIF — the same pairing [`super::inet_control`] declares.
//! Every golden is the **oracle's** own output, measured with `differential run <id>` (never a
//! hand-written guess).
//!
//! # Determinism (no volatile value is pinned, no case can hang)
//!
//! Every listener and every `gen_udp` socket is bound to **loopback only** (`{ip, {127, 0, 0,
//! 1}}`) on port `0`, and the ephemeral port the kernel picks is never part of a golden: it is
//! bound to a variable and used only as a `connect`/`send` argument. The acceptance criterion of
//! every *received* payload is the payload the case itself sent (a byte image, a byte count, a
//! byte sum, or `=:=` against the sent term), so nothing host-dependent leaks into a golden — no
//! port, no address, no clock, no temp path, no pid. Every socket is closed before the row
//! answers, and every wait is a bounded `receive … after N -> <no data> end` whose **oracle**
//! answer is the golden: a delivery that never happened shows up as the `timeout`/short-count
//! answer, i.e. as a visible mismatch, never as a silent pass and never as a hang.
//!
//! The receiving side is always `{active, true}` and every row drains by **length**
//! (`fun Loop(Acc) when byte_size(Acc) >= N -> Acc; …`), not by message count: TCP is a byte
//! stream, so a single write may be delivered as one message or as several, and the
//! accumulate-until-N loop is invariant under that split while still terminating on its own.
//!
//! # Measured shapes that are deliberately NOT cases (handoffs, never `mismatch`-pinned)
//!
//! Verified in this environment (oracle OTP 29.1.1 / `erts-17.1`; the built `beam_rt`) by
//! running the *same* expression under both runtimes:
//!
//! * **a block-framed stream *write* prepends no length header (`beam_rt`).** This is the one
//!   shape this group would otherwise pin, and it does **not** agree at this commit — measured
//!   on the raw peer socket of a `{active, true}` receiver:
//!   - `{packet, 2}` in `gen_tcp:connect/3`'s option list, `gen_tcp:send(Cl, <<"hello">>)`:
//!     oracle `{ok,{data,<<0,5,104,101,108,108,111>>}}` vs `beam_rt`
//!     `{ok,{data,<<104,101,108,108,111>>}}`;
//!   - `{packet, 4}` via `inet:setopts(Cl, [{packet, 4}])` after the connect: oracle
//!     `{ok,ok,{data,<<0,0,0,5,104,101,108,108,111>>}}` vs `beam_rt`
//!     `{ok,ok,{data,<<104,101,108,108,111>>}}`;
//!   - a `{packet, 4}` sender talking to a `{packet, 4}` receiver: oracle
//!     `{ok,{data,<<104,101,108,108,111>>}}` vs `beam_rt` `{ok,timeout}` (the framed reader
//!     withholds a frame it never got a prefix for);
//!   - `{packet, 1}` misses it the same way, and `{packet, line}` **agrees** (line mode writes
//!     no prefix — that is the `line_mode_send_adds_no_prefix` row).
//!
//!   A missing prefix is a plain observable, so a case would commit a `mismatch`; the guard test
//!   [`no_case_pins_a_measured_divergence`] rejects `{packet, 1|2|4}` outright, and the
//!   measurement is recorded in `divergences.md` §A.4. **The framed-write rows land the moment
//!   the write-side fix does** — they are two `{packet, 4}` / `{packet, 2}` rows that send
//!   `<<"hello">>` to a raw peer and pin the received image (`<<0,0,0,5,104,101,108,108,111>>` /
//!   `<<0,5,104,101,108,108,111>>`), plus a large payload whose 4-byte header needs the
//!   `<<0,0,0,N>>` form; the only edit needed is the golden and this section. Owner: `beam_io` /
//!   `beam_rt`.
//! * **a passive read has nothing to read from (`beam_rt`).** Every *passive* read —
//!   `gen_tcp:recv(A, 0, 2000)` with the bytes already queued by a completed `send`, and
//!   `gen_tcp:recv(A, 2, 1000)` — makes the Rust runtime exit `3` with
//!   `beam_rt: unsupported: opcode: run: process blocked (no message available)` where the
//!   oracle answers `{ok, <<"hi">>}`. That is a typed capability gap, not a divergence, so every
//!   row of this group receives on the *active* path (`{active, true}` + a bounded `receive`)
//!   instead. Owner: `beam_rt` / `beam_io`.
//! * **`gen_tcp:send/2` after `gen_tcp:shutdown(Cl, write)` blocks (`beam_rt`).** The oracle
//!   answers `{ok,{error,closed},{error,enotconn}}` for the two sends after the shutdown; the
//!   Rust runtime takes the same `process blocked` exit `3` path, so the write-after-`shutdown`
//!   half is not pinnable. `gen_tcp:shutdown/2`'s own answers are pinned elsewhere
//!   ([`super::inet_control`]'s `tcp_bare_refusals`). Owner: `beam_rt` / `beam_io`.
//! * **the send-side counters stay zero (`beam_rt`).** `inet:getstat(Cl, [send_cnt, send_oct])`
//!   answers `{ok,[{send_cnt,0},{send_oct,0}]}` before the writes on both sides, but after two
//!   completed writes the oracle answers `{ok,[{send_cnt,2},{send_oct,6}]}` while `beam_rt`
//!   still answers `{ok,[{send_cnt,0},{send_oct,0}]}`: the driver accounting is not updated.
//!   Recorded in `divergences.md` §A.4 and rejected by the guard test. Owner: `beam_io`.
//! * **a bare-integer payload is written by the oracle and dropped by `beam_rt`.**
//!   `gen_tcp:send(Cl, 42)` answers `ok` on both sides, but the oracle puts the single byte `42`
//!   on the wire (the peer receives `<<42,0,255,1,128>>` for the sequence below) where `beam_rt`
//!   writes nothing (`<<0,255,1,128>>`). The neighbouring bad-iodata shapes *do* agree
//!   (`[256]` and `[<<"ab">> | 7]` are both `{error, einval}`), but the oracle also writes a
//!   racy, timestamped `=ERROR REPORT==== … Bad value on output port 'tcp_inet'` line to
//!   **stdout**, which makes any case around them unpinnable anyway. Guarded by
//!   [`no_case_sends_a_bare_integer_payload`]; the byte-transparency row asserts its own bytes
//!   through a *binary* payload instead. Owner: `beam_rt` / `beam_io`.
//!
//! The structural half of A.4 (that a C translation unit referencing the `driver_*` /
//! `erl_drv_*` declarations still compiles against the frozen headers) is asserted in
//! `tests/abi.rs`.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `inet_write/*` eval case.
///
/// [`Probe::Value`] prints the expression's own answer as a readable line, and the golden is
/// the **oracle's** exact stdout line — measured with `differential run <id>`, never
/// hand-written. Every case exits `0`: each refusal is a plain `{error, posix()}` reply, so
/// nothing is left to escape.
fn case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.4 inet write-path group: `inet_write/*`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- the payload the writer is handed: a deep iolist vs a flat binary ----------------
        case(
            "inet_write/iolist_payload_is_concatenated",
            "a gen_tcp:send/2 of the deep iolist [<<\"ab\">>,[$c,$d],[<<\"e\">>,$f],<<\"gh\">>] is delivered as the 8-byte concatenation, byte-identical to the same content sent as one flat binary",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             Drain = fun Loop(Acc) when byte_size(Acc) >= 8 -> Acc; \
                         Loop(Acc) -> receive {tcp, A, Db} -> \
                                          Loop(<<Acc/binary, Db/binary>>) \
                                      after 2000 -> Acc end end, \
             S1 = gen_tcp:send(Cl, [<<\"ab\">>, [$c, $d], [<<\"e\">>, $f], <<\"gh\">>]), \
             D1 = Drain(<<>>), \
             S2 = gen_tcp:send(Cl, <<\"abcdefgh\">>), \
             D2 = Drain(<<>>), \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {S1, D1, S2, D2, D1 =:= D2} end",
            "ok {ok,<<97,98,99,100,101,102,103,104>>,ok,\
             <<97,98,99,100,101,102,103,104>>,true}",
        ),
        // --- several sends arrive as one ordered stream --------------------------------------
        case(
            "inet_write/segmented_sends_arrive_as_the_concatenation",
            "four successive gen_tcp:send/2 calls (<<\"ab\">>, [<<\"cd\">>], [<<>>], <<\"ef\">>) arrive as the ordered concatenation <<\"abcdef\">>, so the writer preserves order and a zero-length send in the middle neither writes nor reorders",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             Drain = fun Loop(Acc) when byte_size(Acc) >= 6 -> Acc; \
                         Loop(Acc) -> receive {tcp, A, Db} -> \
                                          Loop(<<Acc/binary, Db/binary>>) \
                                      after 2000 -> Acc end end, \
             S1 = gen_tcp:send(Cl, <<\"ab\">>), \
             S2 = gen_tcp:send(Cl, [<<\"cd\">>]), \
             S3 = gen_tcp:send(Cl, [<<>>]), \
             S4 = gen_tcp:send(Cl, <<\"ef\">>), \
             D = Drain(<<>>), \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {S1, S2, S3, S4, D} end",
            "ok {ok,ok,ok,ok,<<97,98,99,100,101,102>>}",
        ),
        // --- the writer is byte-transparent --------------------------------------------------
        case(
            "inet_write/byte_transparent_payload",
            "a 256-byte payload holding every byte value 0..255 (list_to_binary(lists:seq(0,255))) is delivered back byte-identical, so the writer escapes and transforms nothing",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             Payload = list_to_binary(lists:seq(0, 255)), \
             N = byte_size(Payload), \
             Drain = fun Loop(Acc) when byte_size(Acc) >= N -> Acc; \
                         Loop(Acc) -> receive {tcp, A, Db} -> \
                                          Loop(<<Acc/binary, Db/binary>>) \
                                      after 2000 -> Acc end end, \
             Sn = gen_tcp:send(Cl, Payload), \
             D = Drain(<<>>), \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {Sn, N, byte_size(D), D =:= Payload, binary:part(D, 0, 4)} end",
            "ok {ok,256,256,true,<<0,1,2,3>>}",
        ),
        // --- the writer's answer matrix ------------------------------------------------------
        case(
            "inet_write/write_answer_matrix",
            "gen_tcp:send/2's refusal answers against one listener/connection pair: {error,enotconn} on the listening socket, {error,closed} on the connected socket once close/1 ran, and {error,enotconn}/{error,closed} after the listener is closed too",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             ListenSent = gen_tcp:send(L, <<\"x\">>), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = gen_tcp:close(A), \
             ok = gen_tcp:close(Cl), \
             ClosedSend = gen_tcp:send(Cl, <<\"x\">>), \
             ClosedListen = gen_tcp:send(L, <<\"y\">>), \
             ok = gen_tcp:close(L), \
             TwiceSend = gen_tcp:send(Cl, <<\"z\">>), \
             {ListenSent, ClosedSend, ClosedListen, TwiceSend} end",
            "ok {{error,enotconn},{error,closed},{error,enotconn},{error,closed}}",
        ),
        // --- the peer's close makes the writer closed ----------------------------------------
        case(
            "inet_write/send_after_the_peer_close_notification_is_closed",
            "once the client has received its own {tcp_closed,S} teardown message a gen_tcp:send/2 on that socket is {error,closed}, for a flat binary and for an iolist alike (twice in a row)",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, true}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = gen_tcp:close(A), \
             Cls = receive {tcp_closed, Cl} -> closed after 2000 -> timeout end, \
             Sn = gen_tcp:send(Cl, <<\"x\">>), \
             Sn2 = gen_tcp:send(Cl, [<<\"y\">>]), \
             gen_tcp:close(Cl), \
             gen_tcp:close(L), \
             {Cls, Sn, Sn2} end",
            "ok {closed,{error,closed},{error,closed}}",
        ),
        // --- a zero-length write is a no-op success ------------------------------------------
        case(
            "inet_write/empty_payload_is_a_successful_noop",
            "gen_tcp:send/2 of <<>>, of [] and of [<<>>,[]] all answer ok and put no bytes on the stream: the peer receives only the following <<\"hi\">>",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             E1 = gen_tcp:send(Cl, <<>>), \
             E2 = gen_tcp:send(Cl, []), \
             E3 = gen_tcp:send(Cl, [<<>>, []]), \
             ok = gen_tcp:send(Cl, <<\"hi\">>), \
             Got = receive {tcp, A, Db} -> {data, Db} after 2000 -> timeout end, \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {E1, E2, E3, Got} end",
            "ok {ok,ok,ok,{data,<<104,105>>}}",
        ),
        // --- the write-side option store ------------------------------------------------------
        case(
            "inet_write/write_option_store_and_a_completed_write",
            "the connected socket's write-side option store: the {send_timeout,infinity}/{send_timeout_close,false}/{packet_size,0}/{nodelay,false}/{delay_send,false} defaults, the five values echoed back after inet:setopts/2, the {send_timeout,-1} and {packet_size,-2} {error,einval} refusals, the peer's {recbuf,_} reduced to a > 0 integer, and one completed write",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             D0 = inet:getopts(Cl, [send_timeout, send_timeout_close, packet_size, \
                                    nodelay, delay_send]), \
             ok = inet:setopts(Cl, [{send_timeout, 5000}, {send_timeout_close, true}, \
                                    {packet_size, 4096}, {nodelay, true}, \
                                    {delay_send, true}]), \
             D1 = inet:getopts(Cl, [send_timeout, send_timeout_close, packet_size, \
                                    nodelay, delay_send]), \
             BadTo = inet:setopts(Cl, [{send_timeout, -1}]), \
             BadSz = inet:setopts(Cl, [{packet_size, -2}]), \
             ok = inet:setopts(A, [{active, true}]), \
             Sn = gen_tcp:send(Cl, <<\"hi\">>), \
             Got = receive {tcp, A, Db} -> {data, Db} after 2000 -> timeout end, \
             Rcv = case inet:getopts(A, [recbuf]) of \
                       {ok, [{recbuf, N}]} -> {ok, is_integer(N) andalso N > 0}; \
                       Other -> Other \
                   end, \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {D0, D1, BadTo, BadSz, Sn, Got, Rcv} end",
            "ok {{ok,[{send_timeout,infinity},{send_timeout_close,false},\
             {packet_size,0},{nodelay,false},{delay_send,false}]},{ok,[{send_timeout,5000},\
             {send_timeout_close,true},{packet_size,4096},{nodelay,true},{delay_send,true}]},\
             {error,einval},{error,einval},ok,{data,<<104,105>>},{ok,true}}",
        ),
        // --- a large single write is handed over whole ---------------------------------------
        case(
            "inet_write/large_payload_is_fully_received",
            "a 64 KiB payload written by one gen_tcp:send/2 is received in full: the peer drains until the byte count is reached and the row pins 65536 bytes plus the byte sum of the payload the case itself built",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             Payload = binary:copy(<<\"0123456789abcdef\">>, 4096), \
             N = byte_size(Payload), \
             Drain = fun Loop(Got, Sum) when Got >= N -> {Got, Sum}; \
                         Loop(Got, Sum) -> receive {tcp, A, Db} -> \
                                               Loop(Got + byte_size(Db), \
                                                    Sum + lists:sum(binary_to_list(Db))) \
                                           after 2000 -> {Got, Sum} end end, \
             Sn = gen_tcp:send(Cl, Payload), \
             {Tot, Ck} = Drain(0, 0), \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {N, Sn, Tot, Ck} end",
            "ok {65536,ok,65536,4595712}",
        ),
        // --- the writer is not tied to the connecting side -----------------------------------
        case(
            "inet_write/both_directions_of_the_stream_write",
            "the connected pair writes both ways: the client's iolist [<<\"pi\">>,<<\"ng\">>] arrives on the accepted socket as <<\"ping\">> and the accepted socket's <<\"pong\">> arrives on the client",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             ok = inet:setopts(Cl, [{active, true}]), \
             Up = gen_tcp:send(Cl, [<<\"pi\">>, <<\"ng\">>]), \
             AtSrv = receive {tcp, A, D1} -> {data, D1} after 2000 -> timeout1 end, \
             Down = gen_tcp:send(A, <<\"pong\">>), \
             AtCli = receive {tcp, Cl, D2} -> {data, D2} after 2000 -> timeout2 end, \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {Up, AtSrv, Down, AtCli} end",
            "ok {ok,{data,<<112,105,110,103>>},ok,{data,<<112,111,110,103>>}}",
        ),
        // --- line mode on a sender adds nothing ----------------------------------------------
        case(
            "inet_write/line_mode_send_adds_no_prefix",
            "with {packet,line} on the sending socket gen_tcp:send/2 writes the payload unchanged: <<\"hi\\n\">> arrives as those three bytes (EOL included) and <<\"nonl\">> as those four, with inet:getopts/2 echoing the mode",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
             {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             Set = inet:setopts(Cl, [{packet, line}]), \
             Get = inet:getopts(Cl, [packet]), \
             Drain = fun Loop(Acc, Want) when byte_size(Acc) >= Want -> Acc; \
                         Loop(Acc, Want) -> receive {tcp, A, Db} -> \
                                                Loop(<<Acc/binary, Db/binary>>, Want) \
                                            after 2000 -> Acc end end, \
             S1 = gen_tcp:send(Cl, <<\"hi\\n\">>), \
             D1 = Drain(<<>>, 3), \
             S2 = gen_tcp:send(Cl, <<\"nonl\">>), \
             D2 = Drain(<<>>, 4), \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             {Set, Get, S1, D1, S2, D2} end",
            "ok {ok,{ok,[{packet,line}]},ok,<<104,105,10>>,ok,<<110,111,110,108>>}",
        ),
        // --- the datagram writer ---------------------------------------------------------------
        case(
            "inet_write/datagram_write_path",
            "the datagram writer: gen_udp:send/4 of the iolist [<<\"ab\">>,<<\"cd\">>] is one datagram carrying the 4-byte concatenation, a send of <<>> is a deliverable zero-length datagram ({data,<<>>}, not a timeout), and an 8192-byte datagram arrives byte-identical",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, Rd} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, true}]), \
             {ok, RdPort} = inet:port(Rd), \
             {ok, Wr} = gen_udp:open(0, [binary, {active, false}]), \
             S1 = gen_udp:send(Wr, {127, 0, 0, 1}, RdPort, [<<\"ab\">>, <<\"cd\">>]), \
             G1 = receive {udp, Rd, _, _, D1} -> {data, D1} after 2000 -> timeout1 end, \
             S2 = gen_udp:send(Wr, {127, 0, 0, 1}, RdPort, <<>>), \
             G2 = receive {udp, Rd, _, _, D2} -> {data, D2} after 2000 -> timeout2 end, \
             Big = binary:copy(<<\"0123456789abcdef\">>, 512), \
             S3 = gen_udp:send(Wr, {127, 0, 0, 1}, RdPort, Big), \
             G3 = receive {udp, Rd, _, _, D3} -> {size, byte_size(D3), D3 =:= Big} \
                  after 2000 -> timeout3 end, \
             gen_udp:close(Wr), \
             gen_udp:close(Rd), \
             {S1, G1, S2, G2, S3, G3} end",
            "ok {ok,{data,<<97,98,99,100>>},ok,{data,<<>>},ok,{size,8192,true}}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, Contract};

    /// The group is a real A.4 group: ids are `inet_write/`-prefixed, unique and described, and
    /// every case is a self-terminating `eval` case whose golden is a non-empty line at exit `0`
    /// (a refusal is a plain `{error, posix()}` reply, never an exception left to escape).
    #[test]
    fn every_case_is_a_described_exit_zero_inet_write_eval_case() {
        let cases = group();
        assert_eq!(cases.len(), 11, "the group's size is pinned by this test");
        let mut ids = Vec::new();
        for case in &cases {
            assert!(
                case.id.starts_with("inet_write/"),
                "case {} is not in the inet_write/ group",
                case.id
            );
            assert!(
                case.contracts.contains(&Contract::DriverAbi),
                "case {} must cover contract A.4",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {} must exit 0", case.id);
            let golden = case
                .golden
                .stdout
                .expect("every case carries a golden line");
            assert!(!golden.is_empty(), "case {} has an empty golden", case.id);
            assert!(
                !golden.contains('\n'),
                "case {} pins more than one line",
                case.id
            );
            assert!(
                golden.starts_with("ok "),
                "case {}: unexpected golden shape {golden:?}",
                case.id
            );
            assert!(
                case.expr().is_some(),
                "case {} is not an eval case",
                case.id
            );
            ids.push(case.id);
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "duplicate case id in the inet_write/ group"
        );
    }

    /// This group is the **write** direction: every row has to send through one of the two
    /// stream/datagram writers, otherwise it belongs to [`super::inet_control`].
    #[test]
    fn every_case_writes_through_gen_tcp_or_gen_udp() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            assert!(
                expr.contains("gen_tcp:send(") || expr.contains("gen_udp:send("),
                "case {} never calls a send function, so it is not a write-path case",
                case.id
            );
        }
    }

    /// The shapes that were **measured** to diverge (or to be a typed capability gap) stay out of
    /// the group. Pinning any of them would commit a `mismatch` or an `unsupported`.
    ///
    /// * a block framing mode (`{packet, 1|2|4}`) on a sender writes no length header on
    ///   `beam_rt` — the measurement is in the module docs and `divergences.md` §A.4. `line` is
    ///   the one framing mode that agrees and is what `line_mode_send_adds_no_prefix` pins.
    /// * a **passive** read (`gen_tcp:recv/2,3`) is the `process blocked` exit-3 gap on `beam_rt`,
    ///   so every row receives on the `{active, true}` path.
    /// * `gen_tcp:shutdown/2`'s write half is the same gap (the send after it blocks), and the
    ///   shutdown answers themselves are pinned by `inet_control`.
    /// * `inet:getstat/1`'s send-side counters are never updated on `beam_rt`
    ///   (`{send_cnt,2},{send_oct,6}` against `{0,0}`).
    /// * the raw port-command bridge, `prim_inet`'s own verbs and `os:cmd/1` are out of scope for
    ///   the whole gen_tcp/gen_udp layer (they are pinned by the `udp/`, `driver/` and
    ///   `ram_file/` groups; `os:cmd/1` would need an external sender and a host-dependent
    ///   golden).
    #[test]
    fn no_case_pins_a_measured_divergence() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for forbidden in [
                "gen_tcp:recv(",
                "gen_tcp:shutdown(",
                "inet:getstat(",
                "port_command",
                "prim_inet",
                "os:cmd",
            ] {
                assert!(
                    !expr.contains(forbidden),
                    "case {} uses `{forbidden}`, a measured divergence or capability gap \
                     (see the module docs)",
                    case.id
                );
            }
            // A block framing mode on a **sender**: the oracle prefixes the length header, the
            // runtime writes the payload raw. Written compactly as well as spaced, so a future
            // edit cannot slip one past the guard.
            for framed in [
                "{packet, 1}",
                "{packet, 2}",
                "{packet, 4}",
                "{packet,1}",
                "{packet,2}",
                "{packet,4}",
            ] {
                assert!(
                    !expr.contains(framed),
                    "case {} frames a sender (`{framed}`), which writes no length header \
                     (see the module docs)",
                    case.id
                );
            }
        }
    }

    /// `gen_tcp:send/2` of a **bare integer** is the one iodata shape that diverges: the oracle
    /// puts the integer's byte on the wire, `beam_rt` writes nothing (both answer `ok`). Guard
    /// every send call's second argument against it — an integer *inside* an iolist is a byte and
    /// is fine (`[256]` is `{error, einval}` on both sides, `[$c, $d]` is the two bytes).
    #[test]
    fn no_case_sends_a_bare_integer_payload() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for (at, _) in expr.match_indices("gen_tcp:send(") {
                let call = &expr[at + "gen_tcp:send(".len()..];
                let args = call.split(')').next().unwrap_or(call);
                let payload = args.split_once(',').map(|(_, rest)| rest).unwrap_or("");
                let payload = payload.trim();
                assert!(
                    payload.is_empty() || !payload.chars().all(|ch| ch.is_ascii_digit()),
                    "case {} sends the bare integer `{payload}`, which the oracle writes as a \
                     byte and the runtime drops (see the module docs)",
                    case.id
                );
            }
        }
    }

    /// The probe wrapper binds `V`, `C`, `R` and `X`, so an expression that binds one of those
    /// names makes the **oracle** fail to compile the wrapped expression — a guaranteed, silent
    /// false mismatch. Guard the group against a future edit reintroducing one.
    #[test]
    fn no_case_binds_a_probe_wrapper_variable() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            let names: Vec<&str> = expr
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .filter(|token| !token.is_empty())
                .collect();
            for bound in ["V", "C", "R", "X"] {
                assert!(
                    !names.contains(&bound),
                    "case {} binds `{bound}`, which the probe wrapper already binds",
                    case.id
                );
            }
        }
    }

    /// No row may depend on the clock: a `timer:sleep`, or a `receive` with no `after` clause (an
    /// unbounded wait) would make the group's runtime depend on the host's load. A **bounded**
    /// `receive … after N -> <no data> end` is required — its *oracle* answer is the golden, and
    /// a delivery that never happened shows up as a visible mismatch, never as a hang.
    #[test]
    fn no_case_waits_on_a_message_without_a_bound() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            assert!(
                !expr.contains("timer:sleep"),
                "case {} sleeps on the clock, which no inet_write/ row may do",
                case.id
            );
            let receives = expr.matches("receive").count();
            let bounds = expr.matches("after ").count();
            assert!(
                receives == 0 || bounds >= receives,
                "case {} has a `receive` with no `after` bound (an unbounded wait)",
                case.id
            );
        }
    }

    /// Every row closes every socket it opened, so no descriptor leaks into a later case.
    #[test]
    fn every_case_closes_what_it_opens() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            if expr.contains("gen_tcp:listen(") {
                assert!(
                    expr.contains("gen_tcp:close(L)")
                        && expr.matches("gen_tcp:close(").count() >= 3,
                    "case {} does not close its listener and both connection sockets",
                    case.id
                );
            }
            if expr.contains("gen_udp:open(") {
                assert!(
                    expr.matches("gen_udp:close(").count() >= 2,
                    "case {} does not close both datagram sockets",
                    case.id
                );
            }
        }
    }

    /// Nothing host-dependent may reach a golden: every listener and every datagram socket binds
    /// **loopback port `0`** (so the kernel picks the port, which then never appears in an
    /// answer), and no other address literal is used.
    #[test]
    fn every_socket_binds_loopback_port_zero() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for (at, _) in expr.match_indices("gen_tcp:listen(") {
                assert!(
                    expr[at..].starts_with("gen_tcp:listen(0,"),
                    "case {} listens on a fixed port",
                    case.id
                );
            }
            for (at, _) in expr.match_indices("gen_udp:open(") {
                assert!(
                    expr[at..].starts_with("gen_udp:open(0,"),
                    "case {} opens a fixed datagram port",
                    case.id
                );
            }
            let addresses = expr.matches("{ip, ").count();
            let loopback = expr.matches("{ip, {127, 0, 0, 1}}").count();
            assert_eq!(
                addresses, loopback,
                "case {} binds an address other than loopback",
                case.id
            );
            assert_eq!(
                expr.matches("gen_tcp:connect({127, 0, 0, 1},").count(),
                expr.matches("gen_tcp:connect(").count(),
                "case {} connects somewhere other than loopback",
                case.id
            );
            assert_eq!(
                expr.matches("gen_udp:send(Wr, {127, 0, 0, 1},").count(),
                expr.matches("gen_udp:send(").count(),
                "case {} sends a datagram somewhere other than loopback",
                case.id
            );
        }
    }
}
