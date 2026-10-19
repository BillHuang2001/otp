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
//! Contract **A.4** — the built-in inet drivers' *control surface*, driven from the OTP
//! socket modules (`gen_udp`, `gen_tcp`, `inet`) rather than from the bare port BIFs.
//!
//! [`super::udp`] and [`super::ram_file`] pin the port *object* a built-in driver produces
//! (`port_info/1,2`, the byte counters, the close lifecycle). This group pins the layer above
//! it: the control vocabulary `prim_inet` actually sends — the `INET_REQ_*` / `PACKET_REQ_*`
//! option and opcode verbs — as observed through the documented `gen_udp` / `gen_tcp` /
//! `inet` API. An `inet` socket is a `{spawn_driver, "udp_inet" | "tcp_inet"}` port whose every
//! operation is a `port_control/3` opcode plus a `port_command/2` payload; when those verbs are
//! answered, the module-level API answers, and when they are not, it answers `{error, einval}`
//! (`prim_inet`'s own fallback) — so these rows are exactly the A.4 control surface seen
//! end to end. What each row pins:
//!
//! * **`udp_open_and_option_store`** — `gen_udp:open/2` on loopback produces a live socket, and
//!   `inet:getopts/2` / `inet:setopts/2` round trip the `{active, _}` and `{broadcast, _}`
//!   option store in both directions (including the `active` default). `inet:port/1` answers an
//!   integer port, `inet:sockname/1` answers the bound loopback address, and the closed socket
//!   answers `{error, einval}` to every option/address query.
//! * **`udp_bare_refusals`** — the datagram error matrix: a second open of an already bound
//!   local port is `{error, eaddrinuse}`; a send to a wrong-family address is
//!   `{error, eafnosupport}`; a negative `recv` length and an unknown option are
//!   `{error, einval}`; every operation on a closed socket is `{error, closed}` (or
//!   `{error, einval}` for the option path); a second close is `ok` and an unknown `open`
//!   option is `badarg`.
//! * **`tcp_bare_refusals`** — the same for the stream driver: `listen` on an already bound
//!   local port is `{error, eaddrinuse}`, `recv`/`shutdown` on a listening socket are
//!   `{error, enotconn}`, an unknown option is `{error, einval}`, a double close is `ok`, an
//!   `accept` on a closed listener is `{error, closed}`, and an out-of-range port, a negative
//!   port, an unknown listen option and an unknown connect option are all `badarg`.
//! * **`static_spawn_namespace`** — `open_port({spawn_driver, Name}, Opts)` resolves exactly
//!   the statically linked names: `tcp_inet`, `udp_inet` and `ram_file_drv` open live ports
//!   whose `port_info(P, name)` is exactly `{name, Name}`, while `efile` and `fd` — absent from
//!   the oracle's `driver_tab.c` — raise `error:badarg` on **both** sides.
//! * **`packet_framing_write`** — the write-side framing mode: the `{packet, N}` store
//!   (`0` → `4` → `2` → `1`, each `setopts` answering `ok` and each `getopts` echoing the mode),
//!   the independent `{packet_size, N}` limit (default `0`, settable), the `{packet, 17}`
//!   `{error, einval}` refusal (leaving the previous mode in place), and `gen_udp:open/2`
//!   refusing `{packet, _}` in its option list (`badarg`) — the framing mode is an
//!   `inet:setopts/2` setting, never an open option.
//! * **`packet_framing_read`** — the read-side framing vocabulary: every named frame parser
//!   (`raw`, `line`, `asn1`, `sunrm`, `cdr`, `fcgi`, `tpkt`, `http`, `http_bin`, `httph`,
//!   `httph_bin`) round trips through the store unchanged (with `raw` normalised to `0`), and
//!   the numeric forms `0…4` are accepted while `3`/`8`/`16` are `{error, einval}` and leave
//!   the previous mode untouched.
//!
//! * **`packet4_framed_read`** — the framed **read** path: with `{packet, 4}` on the accepted
//!   socket (plus `{active, true}`) the client's `<<0,0,0,5,"hello">>` arrives as one
//!   `{tcp, A, <<"hello">>}` message — the four-byte prefix consumed, the payload handed up as
//!   the whole frame — and `inet:getopts/2` echoes the mode.
//! * **`packet2_framed_read`** — the same for the two-byte prefix: `<<0,5,"hello">>` arrives as
//!   `{tcp, A, <<"hello">>}`.
//! * **`line_framed_read`** — the `line` parser: `<<"hello\r\nworld">>` yields
//!   `{tcp, A, <<"hello\r\n">>}` (the EOL is part of the frame) and the unterminated
//!   `<<"world">>` tail withholds a message, so the bounded `after` answers `timeout2`.
//! * **`packet4_framed_read_buffers_across_segments`** — the frame boundary is the socket, not the
//!   segment: a lone `<<0,0>>` (a truncated prefix) delivers nothing (`partial_pending`), the
//!   completing `<<0,5,"hello",0,0,0,2,"ab">>` delivers `<<"hello">>` and `<<"ab">>` as **two**
//!   messages, and the mode is inherited from the listening socket's `{packet, 4}`
//!   (`inet:getopts/2` on the accepted socket echoes it).
//! * **`tcp_send_listening_and_connected`** — `gen_tcp:send/2`'s two answers against one
//!   listener: `{error, enotconn}` on the listening socket itself, then `ok` on the socket a
//!   `gen_tcp:connect/3` + `gen_tcp:accept/1` produced.
//! * **`tcp_active_delivery_and_peer_close`** — the stream **data** path end to end: with
//!   `{active, true}` on the accepted socket, the client's bytes arrive as one
//!   `{tcp, S, <<"abc">>}` message and the client's close is the `{tcp_closed, S}` teardown
//!   message (both bounded by a 1000 ms `after`).
//! * **`udp_datagram_delivery_across_two_sockets`** — the datagram **read** path: an active-mode
//!   `{udp, S, {127,0,0,1}, FromPort, Data}` delivery from a *second* socket in the same VM, with
//!   the sender's ephemeral port reduced to `is_integer/1`.
//! * **`udp_passive_recv_returns_the_payload`** — the passive half of the same path:
//!   `gen_udp:recv/3` hands back `{ok, {Ip, FromPort, Data}}` (again with the sender port reduced
//!   to `is_integer/1`).
//!
//! Ids are `inet_control/<snake_case_name>`; the contract is A.4 ([`Contract::DriverAbi`]) plus
//! A.7 ([`Contract::OtpApps`]) wherever the observation is the `gen_udp`/`gen_tcp`/`inet`
//! module answer rather than a bare port BIF. Every golden is the **oracle's** own output,
//! measured with `differential run <id>` (never a hand-written guess).
//!
//! # Determinism (no volatile value is pinned, no case can hang)
//!
//! Every socket these rows open is bound to **loopback only** (`{ip, {127, 0, 0, 1}}`, port
//! `0`), and the ephemeral port the kernel hands out is never pinned: it is either reduced to
//! `is_integer/1` + `P > 0`, or used once as an argument (the `eaddrinuse`, closed-socket and
//! data-path rows) where only the resulting `posix()` reason or payload is observed. Every socket
//! and port is closed before the row answers, so no row leaks a descriptor into the next one.
//! The delivery rows wait with a bounded `receive … after N -> timeout end` (1000 ms, or 300 ms
//! for the truncated-frame probe) whose **oracle** answer is the golden, and the bytes they
//! deliver are already queued in the kernel by a *previous, completed* `send`, so neither depends
//! on host load — and a lost datagram, a dropped connection or an unparsed frame would show up as
//! the `timeout` answer, i.e. as a visible mismatch, never as a silent pass.
//!
//! The framed **read** rows send their bytes from a socket left in `{packet, 0}`, with the length
//! prefix written out literally (`<<0,0,0,5,"hello">>`). That is deliberate: the read-side parser
//! is what they pin, and the *write*-side framing — a block framing mode on the sending socket —
//! is a measured divergence (next section).
//!
//! # Measured shapes that are deliberately NOT cases (handoffs, never `mismatch`-pinned)
//!
//! Verified in this environment (oracle OTP 29.1.1 / `erts-17.1`; the built `beam_rt`) by
//! running the *same* expression under both runtimes:
//!
//! * **a framed stream *write* prepends no length header (`beam_rt`).** With a block framing mode
//!   on the **sending** socket — `{packet, 4}` through `gen_tcp:connect/3`'s option list *or* an
//!   `inet:setopts/2` after the connect — `gen_tcp:send(Cl, <<"hello">>)` reaches a raw peer as
//!   `<<0,0,0,5,104,101,108,108,111>>` under the oracle and as `<<104,101,108,108,111>>` under
//!   `beam_rt`: the header is never written. `{packet, 2}` (`<<0,5,…>>` vs `…`) and `{packet, 1}`
//!   (`<<5,…>>` vs `…`) are missing it the same way, so a framed sender talking to a framed
//!   receiver simply times out on the Rust side (measured: oracle
//!   `{ok,{data,<<104,101,108,108,111>>}}`, `beam_rt` `{ok,timeout}`). The **read** side is exact
//!   in all three modes — that is what the four `*_framed_read` rows pin — and `{packet, line}` on
//!   a sender agrees too (line mode adds no prefix). Owner: `beam_io` / `beam_rt` (the write-side
//!   framing). Reported, never pinned: a missing prefix is a plain observable, so a case would
//!   commit a `mismatch`.
//! * **the blocking reads are a typed capability gap (`beam_rt`).** `gen_tcp:recv(A, 2, 1000)`
//!   (with or without queued bytes), `gen_tcp:accept(L, 200)` and `gen_udp:recv(S, 0, 300)`
//!   (with no datagram queued) each make the Rust runtime exit `3` with
//!   `beam_rt: unsupported: opcode: run: process blocked (no message available)` where the oracle
//!   answers `{ok, …}` / `{error, timeout}`. That is `unsupported`, not a divergence, but it is
//!   not an *agreement* either, so no row pins a read with nothing queued. Owners: `beam_rt` /
//!   `beam_io`.
//! * **`erlang:port_command/2` on a socket-less driver port** now agrees and is pinned by the
//!   [`super::udp`] group (`udp/port_command_on_a_socket_less_port_kills_it`,
//!   `udp/port_command_with_a_caller_ref_prefix_keeps_the_port`).
//!
//! The structural half of A.4 (that a C translation unit referencing the `driver_*` /
//! `erl_drv_*` declarations still compiles against the frozen headers) is asserted in
//! `tests/abi.rs`.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `inet_control/*` eval case.
///
/// [`Probe::Value`] prints the expression's own answer as a readable line, and the golden is
/// the **oracle's** exact stdout line — measured with `differential run <id>`, never
/// hand-written. Every case exits `0`: each refusal is either a plain `{error, posix()}` reply
/// or a `catch`ed reason projected *inside* the expression, so nothing is left to escape.
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

/// The `inet_control/*` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- the datagram socket's open + option store ---------------------------------------
        case(
            "inet_control/udp_open_and_option_store",
            "gen_udp:open/2 on loopback yields a live socket whose {active,_}/{broadcast,_} store round trips through inet:getopts/2 and inet:setopts/2, and which answers {error,einval} to every query once closed",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, S} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}]), \
             {ok, P} = inet:port(S), \
             Dflt = inet:getopts(S, [active, broadcast]), \
             ok = inet:setopts(S, [{active, false}, {broadcast, true}]), \
             Off = inet:getopts(S, [active, broadcast]), \
             ok = inet:setopts(S, [{active, once}, {broadcast, false}]), \
             Once = inet:getopts(S, [active, broadcast]), \
             Sock = case inet:sockname(S) of {ok, {IP, N}} -> {ok, {IP, is_integer(N)}}; \
                                            Other -> Other end, \
             Cl = inet:close(S), \
             Closed = {inet:getopts(S, [active]), inet:sockname(S)}, \
             {is_port(S), is_integer(P), P > 0, Dflt, Off, Once, Sock, Cl, Closed} end",
            "ok {true,true,true,{ok,[{active,true},{broadcast,false}]},\
             {ok,[{active,false},{broadcast,true}]},{ok,[{active,once},{broadcast,false}]},\
             {ok,{{127,0,0,1},true}},ok,{{error,einval},{error,einval}}}",
        ),
        // --- the datagram refusal matrix -----------------------------------------------------
        case(
            "inet_control/udp_bare_refusals",
            "the bare udp driver error matrix: eaddrinuse on a bound local port, eafnosupport on a wrong-family send, einval on a negative recv length / unknown option, closed on every operation after the close, ok on a double close and badarg on an unknown open option",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, S} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}]), \
             {ok, P} = inet:port(S), \
             Busy = case gen_udp:open(P, [{ip, {127, 0, 0, 1}}, {reuseaddr, false}]) of \
                        {ok, S2} -> inet:close(S2), ok; \
                        Other -> Other end, \
             Family = gen_udp:send(S, {0, 0, 0, 0, 0, 0, 0, 1}, 9, <<\"x\">>), \
             BadLen = gen_udp:recv(S, -1), \
             BadOpt = inet:getopts(S, [bogus]), \
             ok = inet:close(S), \
             ClosedSend = gen_udp:send(S, {127, 0, 0, 1}, 9, <<\"x\">>), \
             ClosedRecv = gen_udp:recv(S, 10), \
             ClosedOpt = inet:setopts(S, [{active, false}]), \
             Twice = inet:close(S), \
             BadOpen = case catch gen_udp:open(0, [bogus]) of \
                           {'EXIT', Reason} -> Reason; \
                           Other2 -> Other2 end, \
             {Busy, Family, BadLen, BadOpt, ClosedSend, ClosedRecv, ClosedOpt, Twice, \
              BadOpen} end",
            "ok {{error,eaddrinuse},{error,eafnosupport},{error,einval},{error,einval},\
             {error,closed},{error,closed},{error,einval},ok,badarg}",
        ),
        // --- the stream refusal matrix -------------------------------------------------------
        case(
            "inet_control/tcp_bare_refusals",
            "the bare tcp driver error matrix: eaddrinuse on a bound local port, enotconn on recv/shutdown of a listening socket, einval on an unknown option, ok on a double close, closed on accept after the close, and badarg on an out-of-range port, a negative port and an unknown listen/connect option",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}]), \
             {ok, P} = inet:port(L), \
             Busy = case gen_tcp:listen(P, [{ip, {127, 0, 0, 1}}, {reuseaddr, false}]) of \
                        {ok, L2} -> gen_tcp:close(L2), ok; \
                        Other -> Other end, \
             Recv = gen_tcp:recv(L, 0), \
             Shut = gen_tcp:shutdown(L, read_write), \
             BadOpt = inet:getopts(L, [bogus]), \
             ok = gen_tcp:close(L), \
             Twice = gen_tcp:close(L), \
             Accept = gen_tcp:accept(L), \
             BadPort = case catch gen_tcp:listen(70000, [{ip, {127, 0, 0, 1}}]) of \
                           {'EXIT', R1} -> R1; \
                           {ok, L3} -> gen_tcp:close(L3), unexpected_ok; \
                           Other1 -> Other1 end, \
             NegPort = case catch gen_tcp:listen(-5, [{ip, {127, 0, 0, 1}}]) of \
                           {'EXIT', R2} -> R2; \
                           {ok, L4} -> gen_tcp:close(L4), unexpected_ok; \
                           Other2 -> Other2 end, \
             BadListenOpt = case catch gen_tcp:listen(0, [bogus]) of \
                                {'EXIT', R3} -> R3; \
                                {ok, L5} -> gen_tcp:close(L5), unexpected_ok; \
                                Other3 -> Other3 end, \
             BadConnOpt = case catch gen_tcp:connect({127, 0, 0, 1}, 9, [bogus]) of \
                              {'EXIT', R4} -> R4; \
                              Other4 -> Other4 end, \
             {Busy, Recv, Shut, BadOpt, Twice, Accept, BadPort, NegPort, BadListenOpt, \
              BadConnOpt} end",
            "ok {{error,eaddrinuse},{error,enotconn},{error,enotconn},{error,einval},ok,\
             {error,closed},badarg,badarg,badarg,badarg}",
        ),
        // --- the stream listener's address / option view ---------------------------------------
        case(
            "inet_control/tcp_listen_sockname_and_option_shape",
            "a loopback gen_tcp:listen(0,[{ip,{127,0,0,1}}]) listener is a live port whose inet:port/1 answers an integer, whose inet:sockname/1 answers the loopback address with the ephemeral port reduced to is_integer/1, whose inet:getopts/2 echoes the {active,true}/{packet,0} defaults, and whose unknown-option query is {error,einval}",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}]), \
             {ok, Pn} = inet:port(L), \
             Sock = case inet:sockname(L) of {ok, {IP, N}} -> {ok, {IP, is_integer(N)}}; \
                                            Other -> Other end, \
             Opts = inet:getopts(L, [active, packet]), \
             Bad = inet:getopts(L, [bogus]), \
             ok = gen_tcp:close(L), \
             {is_port(L), is_integer(Pn), Sock, Opts, Bad} end",
            "ok {true,true,{ok,{{127,0,0,1},true}},{ok,[{active,true},{packet,0}]},\
             {error,einval}}",
        ),
        // --- the statically linked driver-name namespace --------------------------------------
        case(
            "inet_control/static_spawn_namespace",
            "open_port({spawn_driver,Name},Opts) resolves exactly the statically linked names: tcp_inet/udp_inet/ram_file_drv open live ports whose port_info(P,name) is exactly {name,Name}, while efile and fd raise error:badarg on both runtimes",
            &[Contract::DriverAbi],
            "[begin Resp = try open_port({spawn_driver, D}, [binary]) \
                          catch error:Reason -> Reason end, \
              case Resp of \
                  Bad when is_atom(Bad) -> {list_to_atom(D), Bad}; \
                  Pt when is_port(Pt) -> \
                      Same = erlang:port_info(Pt, name) =:= {name, D}, \
                      true = erlang:port_close(Pt), \
                      {list_to_atom(D), Same} \
              end end \
              || D <- [\"tcp_inet\", \"udp_inet\", \"ram_file_drv\", \"efile\", \"fd\"]]",
            "ok [{tcp_inet,true},{udp_inet,true},{ram_file_drv,true},{efile,badarg},\
             {fd,badarg}]",
        ),
        // --- the write-side framing mode ------------------------------------------------------
        case(
            "inet_control/packet_framing_write",
            "the write-side {packet,N} framing store: 0 -> 4 -> 2 -> 1 round trips through inet:setopts/2 + inet:getopts/2 with the independent {packet_size,N} limit, {packet,17} is {error,einval} and leaves the mode untouched, and gen_udp:open/2 refuses {packet,_} in its option list with badarg",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, S} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}]), \
             M0 = inet:getopts(S, [packet]), \
             Ok4 = inet:setopts(S, [{packet, 4}]), \
             M4 = inet:getopts(S, [packet]), \
             Ok2 = inet:setopts(S, [{packet, 2}]), \
             M2 = inet:getopts(S, [packet]), \
             Ok1 = inet:setopts(S, [{packet, 1}]), \
             M1 = inet:getopts(S, [packet]), \
             Sz0 = inet:getopts(S, [packet_size]), \
             OkSz = inet:setopts(S, [{packet_size, 100}]), \
             Sz100 = inet:getopts(S, [packet_size]), \
             Bad = inet:setopts(S, [{packet, 17}]), \
             AfterBad = inet:getopts(S, [packet]), \
             inet:close(S), \
             OpenPacket = case catch gen_udp:open(0, [{ip, {127, 0, 0, 1}}, {packet, 4}]) of \
                              {'EXIT', R1} -> R1; \
                              {ok, S9} -> inet:close(S9), unexpected_ok; \
                              Other -> Other end, \
             {M0, Ok4, M4, Ok2, M2, Ok1, M1, Sz0, OkSz, Sz100, Bad, AfterBad, \
              OpenPacket} end",
            "ok {{ok,[{packet,0}]},ok,{ok,[{packet,4}]},ok,{ok,[{packet,2}]},ok,\
             {ok,[{packet,1}]},{ok,[{packet_size,0}]},ok,{ok,[{packet_size,100}]},\
             {error,einval},{ok,[{packet,1}]},badarg}",
        ),
        // --- the read-side framing vocabulary -------------------------------------------------
        case(
            "inet_control/packet_framing_read",
            "the read-side framing vocabulary: every named frame parser (raw/line/asn1/sunrm/cdr/fcgi/tpkt/http/http_bin/httph/httph_bin) round trips through the {packet,_} store unchanged with raw normalised to 0, and the numeric forms accept 0..4 while 3/8/16 are {error,einval} and leave the previous mode in place",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, S} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}]), \
             Named = [{T, inet:setopts(S, [{packet, T}]), inet:getopts(S, [packet])} \
                      || T <- [raw, line, asn1, sunrm, cdr, fcgi, tpkt, http, http_bin, \
                               httph, httph_bin]], \
             Numeric = [{N, inet:setopts(S, [{packet, N}]), inet:getopts(S, [packet])} \
                        || N <- [0, 1, 2, 3, 4, 8, 16]], \
             inet:close(S), \
             {Named, Numeric} end",
            "ok {[{raw,ok,{ok,[{packet,0}]}},{line,ok,{ok,[{packet,line}]}},\
             {asn1,ok,{ok,[{packet,asn1}]}},{sunrm,ok,{ok,[{packet,sunrm}]}},\
             {cdr,ok,{ok,[{packet,cdr}]}},{fcgi,ok,{ok,[{packet,fcgi}]}},\
             {tpkt,ok,{ok,[{packet,tpkt}]}},{http,ok,{ok,[{packet,http}]}},\
             {http_bin,ok,{ok,[{packet,http_bin}]}},{httph,ok,{ok,[{packet,httph}]}},\
             {httph_bin,ok,{ok,[{packet,httph_bin}]}}],[{0,ok,{ok,[{packet,0}]}},\
             {1,ok,{ok,[{packet,1}]}},{2,ok,{ok,[{packet,2}]}},\
             {3,{error,einval},{ok,[{packet,2}]}},{4,ok,{ok,[{packet,4}]}},\
             {8,{error,einval},{ok,[{packet,4}]}},{16,{error,einval},{ok,[{packet,4}]}}]}",
        ),
        // --- the framed read path: the length-prefixed stream ---------------------------------
        case(
            "inet_control/packet4_framed_read",
            "with {packet,4} on the accepted socket the client's <<0,0,0,5,\"hello\">> bytes are delivered as one {tcp,A,<<\"hello\">>} message with the four-byte prefix consumed, and inet:getopts/2 echoes the mode",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             SetR = inet:setopts(A, [{packet, 4}, {active, true}]), \
             GetR = inet:getopts(A, [packet]), \
             Sent = gen_tcp:send(Cl, <<0, 0, 0, 5, \"hello\">>), \
             Got = receive {tcp, A, Db} -> {data, Db} after 2000 -> timeout end, \
             ok = gen_tcp:close(Cl), \
             ok = gen_tcp:close(A), \
             ok = gen_tcp:close(L), \
             {SetR, GetR, Sent, Got} end",
            "ok {ok,{ok,[{packet,4}]},ok,{data,<<104,101,108,108,111>>}}",
        ),
        // --- the same path with the two-byte prefix ------------------------------------------
        case(
            "inet_control/packet2_framed_read",
            "with {packet,2} on the accepted socket the client's <<0,5,\"hello\">> bytes are delivered as one {tcp,A,<<\"hello\">>} message with the two-byte prefix consumed",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{packet, 2}, {active, true}]), \
             Sent = gen_tcp:send(Cl, <<0, 5, \"hello\">>), \
             Got = receive {tcp, A, Db} -> {data, Db} after 2000 -> timeout end, \
             ok = gen_tcp:close(Cl), \
             ok = gen_tcp:close(A), \
             ok = gen_tcp:close(L), \
             {Sent, Got} end",
            "ok {ok,{data,<<104,101,108,108,111>>}}",
        ),
        // --- the line parser over TCP --------------------------------------------------------
        case(
            "inet_control/line_framed_read",
            "with {packet,line} on the accepted socket the client's <<\"hello\\r\\nworld\">> yields one {tcp,A,<<\"hello\\r\\n\">>} message carrying the EOL, and the unterminated <<\"world\">> tail delivers nothing (the bounded after answers timeout2)",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{packet, line}, {active, true}]), \
             Sent = gen_tcp:send(Cl, <<\"hello\\r\\nworld\">>), \
             First = receive {tcp, A, D1} -> {data, D1} after 2000 -> timeout1 end, \
             Second = receive {tcp, A, D2} -> {data, D2} after 2000 -> timeout2 end, \
             ok = gen_tcp:close(Cl), \
             ok = gen_tcp:close(A), \
             ok = gen_tcp:close(L), \
             {Sent, First, Second} end",
            "ok {ok,{data,<<104,101,108,108,111,13,10>>},timeout2}",
        ),
        // --- the frame boundary is the socket, not the segment -------------------------------
        case(
            "inet_control/packet4_framed_read_buffers_across_segments",
            "the {packet,4} parser buffers across segments: a lone <<0,0>> truncated prefix delivers nothing, the completing <<0,5,\"hello\",0,0,0,2,\"ab\">> delivers <<\"hello\">> and <<\"ab\">> as two messages, and the mode is inherited from the listening socket's {packet,4}",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, false}, {packet, 4}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             GetR = inet:getopts(A, [packet]), \
             ok = inet:setopts(A, [{active, true}]), \
             Partial = gen_tcp:send(Cl, <<0, 0>>), \
             Pending = receive {tcp, A, Dp} -> {data, Dp} \
                       after 300 -> partial_pending end, \
             Rest = gen_tcp:send(Cl, <<0, 5, \"hello\", 0, 0, 0, 2, \"ab\">>), \
             First = receive {tcp, A, D1} -> {data, D1} after 2000 -> timeout1 end, \
             Second = receive {tcp, A, D2} -> {data, D2} after 2000 -> timeout2 end, \
             ok = gen_tcp:close(Cl), \
             ok = gen_tcp:close(A), \
             ok = gen_tcp:close(L), \
             {GetR, Partial, Pending, Rest, First, Second} end",
            "ok {{ok,[{packet,4}]},ok,partial_pending,ok,{data,<<104,101,108,108,111>>},\
             {data,<<97,98>>}}",
        ),
        // --- the stream send path -------------------------------------------------------------
        case(
            "inet_control/tcp_send_listening_and_connected",
            "gen_tcp:send/2 answers {error,enotconn} on the listening socket itself and ok on the connected socket a gen_tcp:connect/3 + gen_tcp:accept/1 pair produced",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}]), \
             {ok, Port} = inet:port(L), \
             ListenSent = gen_tcp:send(L, <<\"x\">>), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             Acc = case gen_tcp:accept(L) of \
                       {ok, A} -> gen_tcp:close(A), accepted; \
                       Other -> Other \
                   end, \
             ConnSent = gen_tcp:send(Cl, <<\"hi\">>), \
             gen_tcp:close(Cl), \
             gen_tcp:close(L), \
             {ListenSent, Acc, ConnSent} end",
            "ok {{error,enotconn},accepted,ok}",
        ),
        // --- the stream data path: active-mode delivery and the peer-close teardown ----------
        case(
            "inet_control/tcp_active_delivery_and_peer_close",
            "the connected stream's data path: with {active,true} on the accepted socket the client's bytes arrive as one {tcp,S,<<\"abc\">>} message, and the client's close arrives as the {tcp_closed,S} teardown message",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, \
                                        [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             ok = gen_tcp:send(Cl, <<\"abc\">>), \
             Data = receive {tcp, A, Db} -> {data, Db} after 1000 -> timeout end, \
             ok = gen_tcp:close(Cl), \
             Closed = receive {tcp_closed, A} -> closed after 1000 -> timeout2 end, \
             ok = gen_tcp:close(A), \
             ok = gen_tcp:close(L), \
             {Data, Closed} end",
            "ok {{data,<<97,98,99>>},closed}",
        ),
        // --- the datagram data path: active delivery and the passive read ---------------------
        case(
            "inet_control/udp_datagram_delivery_across_two_sockets",
            "an active-mode datagram sent from a second socket in the same VM is delivered as {udp,S,{127,0,0,1},FromPort,Data} with the sender's ephemeral port reduced to is_integer/1",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, Rd} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}, binary, \
                                              {active, true}]), \
             {ok, RdPort} = inet:port(Rd), \
             {ok, Wr} = gen_udp:open(0, [binary, {active, false}]), \
             Send = gen_udp:send(Wr, {127, 0, 0, 1}, RdPort, <<\"abc\">>), \
             M = receive {udp, Rd, Ip, From, Db} -> {data, Ip, is_integer(From), Db} \
                 after 1000 -> timeout end, \
             gen_udp:close(Wr), \
             gen_udp:close(Rd), \
             {Send, M} end",
            "ok {ok,{data,{127,0,0,1},true,<<97,98,99>>}}",
        ),
        case(
            "inet_control/udp_passive_recv_returns_the_payload",
            "gen_udp:recv/3 on a passive socket hands back {ok,{Ip,FromPort,Data}} for a datagram a second socket in the same VM sent, with the sender's ephemeral port reduced to is_integer/1",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, Rd} = gen_udp:open(0, [{ip, {127, 0, 0, 1}}, binary, \
                                              {active, false}]), \
             {ok, RdPort} = inet:port(Rd), \
             {ok, Wr} = gen_udp:open(0, [binary, {active, false}]), \
             ok = gen_udp:send(Wr, {127, 0, 0, 1}, RdPort, <<\"hi\">>), \
             Rc = case gen_udp:recv(Rd, 0, 1000) of \
                      {ok, {Ip, From, Db}} -> {ok, {Ip, is_integer(From), Db}}; \
                      Other -> Other \
                  end, \
             gen_udp:close(Wr), \
             gen_udp:close(Rd), \
             Rc end",
            "ok {ok,{{127,0,0,1},true,<<104,105>>}}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, Contract};

    /// The group is a real A.4 group: ids are `inet_control/`-prefixed, unique and described,
    /// and every case is a self-terminating `eval` case whose golden is a non-empty line at
    /// exit `0` (a refusal is either a plain `{error, posix()}` reply or caught *inside* the
    /// expression, never left to escape).
    #[test]
    fn every_case_is_a_described_exit_zero_inet_control_eval_case() {
        let cases = group();
        assert_eq!(cases.len(), 15, "the group's size is pinned by this test");
        let mut ids = Vec::new();
        for case in &cases {
            assert!(
                case.id.starts_with("inet_control/"),
                "case {} is not in the inet_control/ group",
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
            "duplicate case id in the inet_control/ group"
        );
    }

    /// The shapes that were **measured** to diverge stay out of the group: a block framing mode
    /// on the **sending** socket (no length header is written by `beam_rt`), the blocking reads
    /// that have nothing queued (a typed `unsupported` capability gap), and `prim_inet`'s own
    /// verbs (not the layer this group drives). Pinning any of them would commit a `mismatch`.
    /// See the module docs for the exact pairs of observations.
    ///
    /// The option/address surface of both drivers (including `inet:getstat/1`,
    /// `gen_tcp:connect/3`, `gen_tcp:listen/2` and the `{mode, list|binary}` store) is served,
    /// and so now are `gen_tcp:send/2` (`{error, enotconn}` on a listener, `ok` on a connection),
    /// the data path of both drivers (raw active-mode delivery, the peer-close teardown and the
    /// passive/active datagram read) and the **framed read** in all three block modes
    /// (`{packet, 4}` / `{packet, 2}` / `line`, including inheritance from `gen_tcp:listen/2` and
    /// buffering across segments), so those verbs are free to appear in a row.
    #[test]
    fn no_case_pins_a_measured_divergence() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for forbidden in [
                // The raw port-command bridge and `prim_inet`'s own verbs: this group drives the
                // documented `gen_udp`/`gen_tcp`/`inet` API, and the bare port BIFs are pinned by
                // the `udp/`, `driver/` and `ram_file/` groups.
                "port_command",
                "prim_inet",
                // The only route to a datagram injected from outside the VM: it needs a
                // non-POSIX sender (`bash`'s `/dev/udp` or a `python3` one-liner), which would
                // make the golden host-dependent.
                "os:cmd",
            ] {
                assert!(
                    !expr.contains(forbidden),
                    "case {} pins `{forbidden}`, a measured divergence (see the module docs)",
                    case.id
                );
            }
            // A block framing mode on the **sending** socket: the oracle prefixes a length header
            // on every `gen_tcp:send/2` (`{packet, 4}` turns `<<"hello">>` into
            // `<<0,0,0,5,"hello">>`), `beam_rt` writes the payload raw — a plain observable, so a
            // case pinning it would be a `mismatch`. Every sender in this group is the socket
            // `gen_tcp:connect/3` produced, so the guard scans that call's option list. A framing
            // mode on a **receiving** socket is fine and is exactly what the `*_framed_read` rows
            // pin (see the module docs and `divergences.md` §A.4).
            for (at, _) in expr.match_indices("gen_tcp:connect(") {
                let call = &expr[at..];
                let opts = call.split(')').next().unwrap_or(call);
                assert!(
                    !opts.contains("{packet, "),
                    "case {} frames the output of its connected socket (`{opts}`), a measured \
                     divergence (see the module docs)",
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
    /// `receive … after N -> timeout end` is allowed — its *oracle* answer is the golden, and a
    /// delivery that never happened shows up as a `timeout` answer, i.e. as a visible mismatch.
    #[test]
    fn no_case_waits_on_a_message() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            assert!(
                !expr.contains("timer:sleep"),
                "case {} sleeps on the clock, which no inet_control/ row may do",
                case.id
            );
            if expr.contains("receive") {
                assert!(
                    expr.contains("after "),
                    "case {} waits on a message with no `after` bound (an unbounded wait)",
                    case.id
                );
            }
        }
    }

    /// Every row closes what it opens: the driver-name, framing, send and data-path rows each
    /// call a close, so no case leaks a socket or port into a later one.
    #[test]
    fn every_case_closes_what_it_opens() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            assert!(
                expr.contains("inet:close(S)")
                    || expr.contains("gen_udp:close(")
                    || expr.contains("gen_tcp:close(L)")
                    || expr.contains("erlang:port_close(Pt)"),
                "case {} never closes the socket/port it opened",
                case.id
            );
        }
    }
}
