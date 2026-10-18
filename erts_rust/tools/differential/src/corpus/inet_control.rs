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
//! Ids are `inet_control/<snake_case_name>`; the contract is A.4 ([`Contract::DriverAbi`]) plus
//! A.7 ([`Contract::OtpApps`]) wherever the observation is the `gen_udp`/`gen_tcp`/`inet`
//! module answer rather than a bare port BIF. Every golden is the **oracle's** own output,
//! measured with `differential run <id>` (never a hand-written guess).
//!
//! # Determinism (no volatile value is pinned, no case can hang)
//!
//! Every socket these rows open is bound to **loopback only** (`{ip, {127, 0, 0, 1}}`, port
//! `0`), and the ephemeral port the kernel hands out is never pinned: it is either reduced to
//! `is_integer/1` + `P > 0`, or used once as an argument (the `eaddrinuse` and closed-socket
//! rows) where only the resulting `posix()` reason is observed. Every socket and port is
//! closed before the row answers, so no row leaks a descriptor into the next one. No row sends
//! a datagram that is expected to be delivered and no row waits for one, so there is **no
//! `receive` and no `after` guard at all** — every row is a straight-line computation over
//! synchronous port-BIF replies, which is also why a loaded host cannot reorder it.
//!
//! # Measured divergences that are deliberately NOT cases (handoffs, never `mismatch`-pinned)
//!
//! The datagram **data** path is one of them, which is why this group pins the framing *mode*
//! rather than the frame *bytes* (`erlang:decode_packet/3` — the same framing algorithm — is
//! already pinned byte-exactly by [`super::decode_packet`]). Each item below is a plain Erlang
//! value/reason (or a typed capability gap), so pinning it would be a `mismatch`; it is
//! reported to the runtime owners instead. Verified in this environment (oracle OTP 29.1.1 /
//! `erts-17.1`; the built `beam_rt`) by running the *same* expression under both runtimes:
//!
//! * **a datagram send never gets its driver reply (`beam_rt`).** `gen_udp:send(S,
//!   {127,0,0,1}, P, <<"hi">>)` answers `ok` under the oracle; here `prim_inet:do_sendto/4`'s
//!   `erlang:port_command/2` answers `true` and the following
//!   `receive {inet_reply, S, Reply, Ref}` never completes — the `-eval` route exits `3` with
//!   `beam_rt: unsupported: opcode: run: process blocked (no message available)` and the
//!   compiled-module route (`beam_rt run`) hangs to its 60 s deadline. A *successful* send
//!   therefore has no pinnable shape, and neither has the length header it would write. Owner:
//!   `beam_rt` (the port-task reply bridge).
//! * **an incoming datagram is never delivered (`beam_rt`).** With a `{active, true}` socket,
//!   a datagram injected from outside the VM (`os:cmd("printf abc > /dev/udp/127.0.0.1/P")`)
//!   arrives as `{udp, S, {127,0,0,1}, _, <<"abc">>}` under the oracle and as nothing at all
//!   under `beam_rt` within a 2 s guard; `gen_udp:recv(S, 0, 300)` likewise answers
//!   `{error, einval}` there and raises `{case_clause, {ok, …}}` (inside
//!   `prim_inet:recvfrom0/3`) here. So no read-path reassembly or truncation refusal can be
//!   pinned. Owner: `beam_rt` / `beam_io`.
//! * **`gen_tcp:accept/2` on a listening socket (`beam_rt`).** The oracle answers
//!   `{error, timeout}` for a short accept timeout; here `prim_inet:async_accept/2` fails with a
//!   `{case_clause, {ok, []}}` — the same `inet:getopts/2` reply shape the other `prim_inet`
//!   callers pattern-match on. Owner: `beam_rt` (the `inet:getopts/2` reply shape).
//! * **`gen_tcp:send/2` on a listening socket (`beam_io`).** The oracle answers
//!   `{error, enotconn}`; `beam_rt` answers `{error, einval}`.
//! * **`erlang:port_command/2` on a socket-less driver port** is measured and unpinnable rather
//!   than divergent: the oracle answers `true` and then kills the port
//!   (`driver_failure_posix(EINVAL)`, a following `port_info/2` is `undefined`), `beam_rt` raises
//!   `error:badarg` and leaves the port alive. Owner: `beam_rt` / `beam_io` (the port-command
//!   bridge).
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
        assert_eq!(cases.len(), 7, "the group's size is pinned by this test");
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

    /// The shapes that were **measured** to diverge stay out of the group: the datagram *data*
    /// path is unreachable from Erlang on `beam_rt` (a send never receives its `inet_reply` and
    /// an incoming datagram is never delivered), `gen_tcp:accept/2` fails inside `prim_inet`, and
    /// `port_command/2` on a socket-less port has no shared observable — pinning any of them
    /// would commit a `mismatch`. See the module docs for the exact pairs of observations.
    ///
    /// The option/address surface of both drivers (including `inet:getstat/1`,
    /// `gen_tcp:connect/3`, `gen_tcp:listen/2` and the `{mode, list|binary}` store) is served,
    /// so those verbs are free to appear in a row.
    #[test]
    fn no_case_pins_a_measured_divergence() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for forbidden in [
                // The send verb family: its `inet_reply` never reaches the owner, and its
                // closing path polls the driver's counters.
                "port_command",
                "prim_inet",
                // The only route to a datagram injected from outside the VM.
                "os:cmd",
                // A stream send on a listening socket: enotconn under the oracle, einval here.
                "gen_tcp:send",
                "ok = gen_udp:send",
            ] {
                assert!(
                    !expr.contains(forbidden),
                    "case {} pins `{forbidden}`, a measured divergence (see the module docs)",
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

    /// No row may wait: an `after` guard (or a bare `receive`) would make the group's runtime
    /// depend on the host's load, and the datagram delivery it would wait for is a measured
    /// `beam_rt` gap anyway (see the module docs).
    #[test]
    fn no_case_waits_on_a_message() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for forbidden in ["receive", "after ", "timer:sleep"] {
                assert!(
                    !expr.contains(forbidden),
                    "case {} waits (`{forbidden}`), which no inet_control/ row may do",
                    case.id
                );
            }
        }
    }

    /// Every row closes what it opens: the driver-name, framing and datagram rows each call a
    /// close, so no case leaks a socket or port into a later one.
    #[test]
    fn every_case_closes_what_it_opens() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            assert!(
                expr.contains("inet:close(S)")
                    || expr.contains("gen_tcp:close(L)")
                    || expr.contains("erlang:port_close(Pt)"),
                "case {} never closes the socket/port it opened",
                case.id
            );
        }
    }
}
