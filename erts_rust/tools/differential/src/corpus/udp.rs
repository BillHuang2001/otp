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
//! Contract **A.4** — the `udp_inet` built-in driver's observable port surface, and the control
//! vocabulary `prim_inet` drives over it.
//!
//! `beam_rt`'s built-in driver registry serves `udp_inet`
//! (`../../crates/beam_io/src/drivers/inet/udp.rs`), so `open_port({spawn_driver,
//! "udp_inet"}, Opts)` must resolve the name to a **real emulator port** exactly as the
//! oracle's statically linked `udp_inet_driver_entry` (`inet_drv.c:1406`) does — and
//! `erlang:port_control/3` must reach that driver's own verbs. What this group pins is both
//! halves of that surface, driven only through the documented `erlang` port BIFs — the program
//! comes from the CLI (contract A.6), the port and its driver are A.4:
//!
//! * **name resolution** — a live port whose `port_info(P, name)` is `{name, "udp_inet"}` in
//!   both the `[binary]` and the default (list) stream mode, and with a `{packet, N}` option in
//!   the option list (the port grammar is the same one every driver port goes through);
//! * **`erlang:ports/0` membership** — the port is a member while it is open and is gone after
//!   `port_close/1`; the count of live `udp_inet` ports is exactly the number opened, so three
//!   concurrently open ports are three ports and closing them leaves zero;
//! * **the `port_info/1,2` view** — the seven-item key set, and the individual items a driver
//!   port answers: `name`, `connected` (the owning process), `os_pid` (`undefined`: a
//!   `{spawn_driver, _}` port has no OS process), `registered_name` (`[]` while unregistered),
//!   `links` (one link, to the owner), `input`/`output` (both zero) and `id` (a non-negative
//!   local integer) — plus the two accounting items the port floor now serves as well:
//!   `queue_size` (`{queue_size, 0}` on a fresh port) and `memory` (a positive word count,
//!   which is implementation-specific and therefore pinned as a **shape**, never a figure);
//! * **the close lifecycle** — `port_close/1` answers `true`, the port leaves `ports/0`, the
//!   `port_info` view becomes `undefined` (both `port_info/1` and every item), a second close and
//!   a `port_command/2` on the closed port are `error:badarg`, and the term is still a port
//!   (`is_port/1`), just not a pid;
//! * **port-term identity** — `list_to_port(port_to_list(P))` is `P`;
//! * **one name per driver** — opening `"udp_inet"` and `"tcp_inet"` in the same run reports
//!   `udp_inet` and `tcp_inet` respectively (a name is never aliased or shared);
//! * **the emulator accounting** — `erlang:system_info(port_count)` grows by exactly one while
//!   the port is open and is back where it started after the close;
//! * **the control verbs** — `erlang:port_control/3` with the **decimal** control code the
//!   Erlang side sends (`INET_REQ_* + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`, i.e. `Op +
//!   16#03f1a300`, `prim_inet.erl`) reaches the driver's `case`: `INET_REQ_OPEN` (1) answers the
//!   `[1]` status list, `INET_REQ_BIND` (6) the `[1, port_hi, port_lo]` the kernel assigned,
//!   `INET_REQ_NAME` (5) the packed local address, `INET_REQ_CLOSE` (2) `[1]` (and the *port*
//!   survives that verb, so the later `port_close/1` still answers `true`), `INET_REQ_GETTYPE`
//!   (15) / `INET_REQ_GETSTATUS` (16) the cached socket kind and the lifecycle state;
//! * **the option store over the wire** — `INET_REQ_SETOPTS` (7) / `INET_REQ_GETOPTS` (8) with
//!   the driver's own `[opt, val_be32]*` records: the `active`(22)/`packet`(23)/
//!   `packet_size`(32)/`mode`(24)/`deliver`(25)/`read_packets`(33) defaults, a multi-record
//!   `setopts` read back in request order, the `{active, N}` multi form's **accumulating**
//!   16-bit counter, and the whole-reply `[0] ++ "einval"` refusal of an unserved code;
//! * **the verb refusals** — an unknown verb (a code inside the magic range that no `case`
//!   matches, the retired request `9`, a raw value *below* the magic, and an out-of-range code)
//!   answers the C's `[0] ++ "exbadport"` reply, identically for every one of them.
//!
//! # Determinism (no socket traffic, no clock, no pid is pinned)
//!
//! `open_port/2` on a `{spawn_driver, Name}` port only builds the driver's descriptor: the
//! oracle's C creates the socket in `INET_REQ_OPEN` (`packet_inet_ctl`, `inet_drv.c:14577`) and
//! this floor's does the same in its `open` verb. One row goes one step further and **binds** a
//! datagram socket to loopback (`INET_REQ_BIND` with the `INET_AF_LOOPBACK` short form, the only
//! way to reach the address verbs) — but **nothing is ever sent**, no listener is created and no
//! inbound datagram is awaited, so there is no loopback traffic to race. The ephemeral port the
//! kernel assigns is never pinned: the bind/name row reduces it to `> 0` and keeps only the
//! loopback octets, which come from the argument itself. Every case closes the ports it opened,
//! so no case leaks a port into the next one. Nothing volatile is asserted: the port term
//! itself, the `port_info(P, id)` index, the owning pid and the `ports/0` length are all
//! specific to one VM, so they are compared **relative** to `self()`/before-open (`=:= self()`,
//! `length(Links)`, `During - Before`) or only by their class (`is_integer/1`, `is_port/1`) —
//! never by value. Every expression terminates on its own with no `receive`, so there is no
//! `after` guard to need (and none is missing).
//!
//! # The datagram *data* path is the remaining capability gap (measured, never allowlisted)
//!
//! The control/option verb transport above works on both runtimes; what `beam_rt` still does not
//! have is the seam between those verbs and the datagram **bytes**. Measured in this environment
//! (oracle OTP 29.1.1 / `erts-17.1`, and the built `beam_rt`) on a socket the `open`/`bind`
//! verbs produced:
//!
//! * **a send is a typed capability gap.** `gen_udp:send(S, {127,0,0,1}, P, <<"hi">>)` answers
//!   `ok` under the oracle, while `beam_rt` aborts the whole run (exit 3) with
//!   `beam_rt: unsupported: the C's inet statistics counters (inet_fill_stat) are not tracked by
//!   this driver`; `inet:getstat/1` is the same gap. The read side is unreachable too:
//!   `gen_udp:recv/3` answers under the oracle and raises `{case_clause, {ok, []}}` inside
//!   `prim_inet` here, and `{active, true}` delivery never produces a datagram at all.
//! * **`erlang:port_command/2` on the socket-less port** (a port whose `INET_REQ_OPEN` was never
//!   sent) is the sharp edge of the same gap: the oracle answers `true` and then kills the port
//!   (`init_caller` fails, `driver_failure_posix(EINVAL)`, `inet_drv.c:15055`, so `port_info/2`
//!   is afterwards `undefined` and a second `port_close/1` is `badarg`), while `beam_rt` raises
//!   `error:badarg` and leaves the port **alive** — the two sides share no observable there, so
//!   this group pins `port_command/2` only on a **closed** port (both sides `badarg`) and the
//!   socket-less pair is reported to the owner instead.
//! * **the stream half of the same driver file** is unreachable too: `gen_tcp:connect/3`
//!   refuses with `badarg` under `beam_rt` where the oracle opens a socket, and
//!   `gen_tcp:listen/2` refuses the list/binary stream-mode option (`{error, einval}`).
//!
//! Owner of all three: `beam_rt` (the built-in-driver `port_command` bridge) / `beam_io` (the
//! driver's `send`/`recv` verbs). No row below pins any of them; the module docs of
//! [`super::inet_control`] carry the full measured matrix.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.4 `udp_inet` eval case.
///
/// [`Probe::Value`] prints the expression's own answer as a readable line (`ok {name, …}`,
/// `ok badarg`), and the golden is the **oracle's** exact stdout line — measured with
/// `differential run <id>`, never hand-written. Every case exits `0`: each refusal shape (a
/// second `port_close/1`, a `port_command/2` on the closed port) is caught *inside* the
/// expression, and the driver's own refusals are ordinary reply lists (`[0] ++ "einval"`,
/// `[0] ++ "exbadport"`), so the observable is a value rather than an uncaught exception.
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

/// The `udp/*` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- name resolution: open_port/2 → a live port named udp_inet -----------------------
        case(
            "udp/opens_a_live_port_named_udp_inet",
            "open_port({spawn_driver,\"udp_inet\"},[binary]) resolves the built-in driver name to a live port reporting {name,\"udp_inet\"}",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Res = {is_port(P), erlang:port_info(P, name)}, \
             erlang:port_close(P), Res end",
            "ok {true,{name,[117,100,112,95,105,110,101,116]}}",
        ),
        case(
            "udp/opens_in_list_mode_with_the_same_name",
            "a udp_inet port opened in the default list mode reports the same {name,\"udp_inet\"}",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, []), \
             Nm = erlang:port_info(P, name), erlang:port_close(P), Nm end",
            "ok {name,[117,100,112,95,105,110,101,116]}",
        ),
        case(
            "udp/accepts_a_port_option_and_keeps_its_name",
            "a udp_inet port opened with [binary,{packet,2}] keeps its {name,\"udp_inet\"} (the port option grammar is unchanged)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary, {packet, 2}]), \
             Nm = erlang:port_info(P, name), erlang:port_close(P), Nm end",
            "ok {name,[117,100,112,95,105,110,101,116]}",
        ),
        case(
            "udp/each_driver_name_reports_its_own_name",
            "opening \"udp_inet\" and \"tcp_inet\" in one run reports udp_inet and tcp_inet respectively (a built-in driver name is never aliased)",
            &[Contract::DriverAbi],
            "[begin P = open_port({spawn_driver, D}, [binary]), \
              Nm = erlang:port_info(P, name), erlang:port_close(P), Nm end \
              || D <- [\"udp_inet\", \"tcp_inet\"]]",
            "ok [{name,[117,100,112,95,105,110,101,116]},{name,[116,99,112,95,105,110,101,116]}]",
        ),
        // --- erlang:ports/0 membership and the emulator's port accounting --------------------
        case(
            "udp/live_port_is_a_member_of_ports",
            "a live udp_inet port is a member of erlang:ports/0",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             M = lists:member(P, erlang:ports()), erlang:port_close(P), M end",
            "ok true",
        ),
        case(
            "udp/closed_port_leaves_ports",
            "a udp_inet port is no longer a member of erlang:ports/0 once port_close/1 has returned",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             true = erlang:port_close(P), lists:member(P, erlang:ports()) end",
            "ok false",
        ),
        case(
            "udp/three_live_ports_are_counted_and_all_closed",
            "three concurrently open udp_inet ports are three members of erlang:ports/0, and closing them leaves none",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin Ps = [open_port({spawn_driver, \"udp_inet\"}, [binary]) || _ <- lists:seq(1, 3)], \
             Live = length([Q || Q <- erlang:ports(), \
                                  erlang:port_info(Q, name) =:= {name, \"udp_inet\"}]), \
             _ = [erlang:port_close(Q) || Q <- Ps], \
             {Live, length([Q || Q <- erlang:ports(), \
                                 erlang:port_info(Q, name) =:= {name, \"udp_inet\"}])} end",
            "ok {3,0}",
        ),
        case(
            "udp/port_count_grows_by_one_while_open",
            "erlang:system_info(port_count) grows by exactly one while a udp_inet port is open and returns to its previous value after the close",
            &[Contract::DriverAbi, Contract::Introspection],
            "begin Before = erlang:system_info(port_count), \
             P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             During = erlang:system_info(port_count), true = erlang:port_close(P), \
             {During - Before, erlang:system_info(port_count) - Before} end",
            "ok {1,0}",
        ),
        // --- the port_info/1,2 view of a live driver port -----------------------------------
        case(
            "udp/port_info_key_set",
            "a live udp_inet port reports exactly the seven port_info/1 keys connected/id/input/links/name/os_pid/output",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Ks = [K || {K, _} <- erlang:port_info(P)], erlang:port_close(P), lists:sort(Ks) end",
            "ok [connected,id,input,links,name,os_pid,output]",
        ),
        case(
            "udp/port_info_connected_names_the_owner_process",
            "port_info(P,connected) of a udp_inet port names the process that opened it",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Cd = erlang:port_info(P, connected), erlang:port_close(P), \
             case Cd of {connected, Owner} -> {connected, Owner =:= self()}; Other -> Other end end",
            "ok {connected,true}",
        ),
        case(
            "udp/port_info_os_pid_is_undefined",
            "port_info(P,os_pid) of a udp_inet driver port is {os_pid,undefined} (it spawns no OS process)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Op = erlang:port_info(P, os_pid), erlang:port_close(P), Op end",
            "ok {os_pid,undefined}",
        ),
        case(
            "udp/port_info_registered_name_is_empty",
            "port_info(P,registered_name) of an unregistered udp_inet port is the bare [] (not undefined, not {registered_name,[]})",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Rn = erlang:port_info(P, registered_name), erlang:port_close(P), Rn end",
            "ok []",
        ),
        case(
            "udp/port_info_links_name_the_owner_process",
            "port_info(P,links) of a udp_inet port is a one-element list holding the owning process",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             {links, Ln} = erlang:port_info(P, links), erlang:port_close(P), \
             {length(Ln), is_list(Ln), lists:member(self(), Ln)} end",
            "ok {1,true,true}",
        ),
        case(
            "udp/port_info_byte_counters_start_at_zero",
            "a freshly opened udp_inet port's port_info(P,input) and port_info(P,output) byte counters are both zero",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Counters = {erlang:port_info(P, input), erlang:port_info(P, output)}, \
             erlang:port_close(P), Counters end",
            "ok {{input,0},{output,0}}",
        ),
        case(
            "udp/port_info_id_is_a_local_integer",
            "the id item of a live udp_inet port is a non-negative local integer (its exact index is VM-specific and is not pinned)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             {id, Id} = erlang:port_info(P, id), erlang:port_close(P), \
             {is_integer(Id), Id >= 0} end",
            "ok {true,true}",
        ),
        // --- the close lifecycle ------------------------------------------------------------
        case(
            "udp/port_info_on_a_closed_port_is_undefined",
            "after port_close/1 both port_info/1 and every port_info/2 item of the udp_inet port answer undefined",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             true = erlang:port_close(P), \
             {erlang:port_info(P), erlang:port_info(P, name), erlang:port_info(P, connected)} end",
            "ok {undefined,undefined,undefined}",
        ),
        case(
            "udp/closing_twice_is_badarg",
            "a second erlang:port_close/1 of an already closed udp_inet port raises error:badarg",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             true = erlang:port_close(P), \
             try erlang:port_close(P) catch error:Reason -> Reason end end",
            "ok badarg",
        ),
        case(
            "udp/a_closed_port_keeps_its_port_term",
            "a closed udp_inet port is still a port term (is_port/1) and never becomes a pid",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             true = erlang:port_close(P), {is_port(P), is_pid(P)} end",
            "ok {true,false}",
        ),
        // --- port-term identity -------------------------------------------------------------
        case(
            "udp/port_to_list_and_list_to_port_round_trip",
            "list_to_port(port_to_list(P)) of a udp_inet port is the same port term",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Back = erlang:list_to_port(erlang:port_to_list(P)), \
             erlang:port_close(P), Back =:= P end",
            "ok true",
        ),
        // --- the control verbs prim_inet drives through port_control/3 ----------------------
        case(
            "udp/port_control_open_reply_is_the_inet_status_list",
            "erlang:port_control/3 with the decimal INET_REQ_OPEN code prim_inet sends (1 + 16#03f1a300) reaches the built-in driver and answers its [1] status reply",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Rep = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             erlang:port_close(P), Rep end",
            "ok [1]",
        ),
        case(
            "udp/port_control_getopts_reports_the_stored_options",
            "INET_REQ_GETOPTS (8 + 16#03f1a300) reports the driver-state option defaults as [1] ++ [opt, val_be32]* in request order: active 0, packet 0, packet_size 0, mode 0, deliver 1, read_packets 5",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             _ = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             Rep = erlang:port_control(P, 16#03f1a300 + 8, <<22,23,32,24,25,33>>), \
             erlang:port_close(P), Rep end",
            "ok [1,22,0,0,0,0,23,0,0,0,0,32,0,0,0,0,24,0,0,0,0,25,0,0,0,1,33,0,0,0,5]",
        ),
        case(
            "udp/port_control_setopts_store_and_readback",
            "INET_REQ_SETOPTS (7 + 16#03f1a300) storing three [opt, val_be32] records (active 1, packet 4, packet_size 300) answers [1] and each value reads back in order, while an unserved option code is the whole-reply [0] ++ \"einval\"",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             _ = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             S = erlang:port_control(P, 16#03f1a300 + 7, \
                                     <<22,0,0,0,1, 23,0,0,0,4, 32,0,0,1,44>>), \
             G = erlang:port_control(P, 16#03f1a300 + 8, <<22,23,32>>), \
             Bad = erlang:port_control(P, 16#03f1a300 + 8, <<200>>), \
             erlang:port_close(P), {S, G, Bad} end",
            "ok {[1],[1,22,0,0,0,1,23,0,0,0,4,32,0,0,1,44],[0,101,105,110,118,97,108]}",
        ),
        case(
            "udp/port_control_active_multi_accumulates_its_counter",
            "the {active, N} multi form (value 3 plus a trailing 16-bit counter) accumulates its counter across setopts (2 then +3 = 5), and setting a plain {active, true} resets it to the bare record",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             _ = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             S1 = erlang:port_control(P, 16#03f1a300 + 7, <<22,0,0,0,3,0,2>>), \
             G1 = erlang:port_control(P, 16#03f1a300 + 8, <<22>>), \
             S2 = erlang:port_control(P, 16#03f1a300 + 7, <<22,0,0,0,3,0,3>>), \
             G2 = erlang:port_control(P, 16#03f1a300 + 8, <<22>>), \
             S3 = erlang:port_control(P, 16#03f1a300 + 7, <<22,0,0,0,1>>), \
             G3 = erlang:port_control(P, 16#03f1a300 + 8, <<22>>), \
             erlang:port_close(P), {S1, G1, S2, G2, S3, G3} end",
            "ok {[1],[1,22,0,0,0,3,0,2],[1],[1,22,0,0,0,3,0,5],[1],[1,22,0,0,0,1]}",
        ),
        case(
            "udp/port_control_bind_reports_the_loopback_address",
            "INET_REQ_BIND (6 + 16#03f1a300) with the INET_AF_LOOPBACK short form and port 0 answers [1, port_hi, port_lo] with a kernel-assigned non-zero port, and INET_REQ_NAME (5) then reports that port with the loopback octets (the ephemeral port itself is not pinned)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             _ = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             Bind = erlang:port_control(P, 16#03f1a300 + 6, <<4,0,0>>), \
             BShape = case Bind of \
                          [Tag, Hi, Lo] -> {length(Bind), Tag, Hi * 256 + Lo > 0}; \
                          Other -> Other \
                      end, \
             Nm = erlang:port_control(P, 16#03f1a300 + 5, <<>>), \
             NShape = case Nm of \
                          [Tg, Fam, Hi2, Lo2, O1, O2, O3, O4] -> \
                              {length(Nm), Tg, Fam, {O1, O2, O3, O4}, \
                               Hi2 * 256 + Lo2 > 0}; \
                          Other2 -> Other2 \
                      end, \
             erlang:port_close(P), {BShape, NShape} end",
            "ok {{3,1,true},{8,1,1,{127,0,0,1},true}}",
        ),
        case(
            "udp/port_control_gettype_and_getstatus",
            "INET_REQ_GETTYPE (15) reports the cached socket kind ([0] ++ \"einval\" before the open, the AF_INET/datagram pair after it) and INET_REQ_GETSTATUS (16) the lifecycle state ([1,0,0,0,0] closed, [1,0,0,0,1] open)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             T0 = erlang:port_control(P, 16#03f1a300 + 15, <<>>), \
             S0 = erlang:port_control(P, 16#03f1a300 + 16, <<>>), \
             _ = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             T1 = erlang:port_control(P, 16#03f1a300 + 15, <<>>), \
             S1 = erlang:port_control(P, 16#03f1a300 + 16, <<>>), \
             erlang:port_close(P), {T0, S0, T1, S1} end",
            "ok {[0,101,105,110,118,97,108],[1,0,0,0,0],[1,0,0,0,1,0,0,0,2],[1,0,0,0,1]}",
        ),
        case(
            "udp/port_control_close_is_accepted",
            "INET_REQ_CLOSE (2 + 16#03f1a300) answers the driver's [1], and it closes only the driver's socket: the port is still live (port_info(P,name) unchanged) and the following port_close/1 still answers true",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             _ = erlang:port_control(P, 16#03f1a300 + 1, <<1,2,0>>), \
             Rep = erlang:port_control(P, 16#03f1a300 + 2, <<>>), \
             Alive = erlang:port_info(P, name), \
             Closed = erlang:port_close(P), {Rep, Alive, Closed} end",
            "ok {[1],{name,[117,100,112,95,105,110,101,116]},true}",
        ),
        case(
            "udp/port_control_with_an_unknown_verb_is_refused",
            "every unknown control verb — an in-range code no case matches (42), the retired request (9), a raw value below the magic (99) and an out-of-range code (1000000 + magic) — answers the identical C reply [0] ++ \"exbadport\" rather than raising",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             Reps = [erlang:port_control(P, 16#03f1a300 + Op, <<>>) \
                     || Op <- [42, 9, 1000000]], \
             Raw = erlang:port_control(P, 99, <<>>), \
             All = lists:usort([Raw | Reps]), \
             erlang:port_close(P), {length(All), hd(All)} end",
            "ok {1,[0,101,120,98,97,100,112,111,114,116]}",
        ),
        // --- the port_info accounting items and the port_command refusal --------------------
        case(
            "udp/port_info_queue_size_and_memory_of_a_fresh_port",
            "port_info(P,queue_size) of a fresh udp_inet port is the empty-queue {queue_size,0} and port_info(P,memory) a positive word count (its figure is implementation-specific and is pinned as a shape only)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             {queue_size, Q} = erlang:port_info(P, queue_size), \
             {memory, Mem} = erlang:port_info(P, memory), \
             erlang:port_close(P), {Q, is_integer(Mem), Mem > 0} end",
            "ok {0,true,true}",
        ),
        case(
            "udp/port_command_on_a_closed_port_is_badarg",
            "erlang:port_command/2 on an already closed udp_inet port raises error:badarg on both runtimes (caught inside the expression, so the observable is the reason atom)",
            &[Contract::DriverAbi],
            "begin P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
             true = erlang:port_close(P), \
             try erlang:port_command(P, <<\"hi\">>) catch error:Reason -> Reason end end",
            "ok badarg",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, Contract};

    /// The group is a real A.4 group: ids are `udp/`-prefixed, unique and described, and every
    /// case is a self-terminating `eval` case whose golden is a non-empty line at exit `0`
    /// (a refusal is caught *inside* the expression, never left to escape).
    #[test]
    fn every_case_is_a_described_exit_zero_udp_eval_case() {
        let cases = group();
        assert_eq!(cases.len(), 29, "the group's size is pinned by this test");
        let mut ids = Vec::new();
        for case in &cases {
            assert!(
                case.id.starts_with("udp/"),
                "case {} is not in the udp/ group",
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
            "duplicate case id in the udp/ group"
        );
    }

    /// The shapes that are **measured** to diverge stay out of the group: the datagram *data*
    /// path (a `gen_udp` send and `inet:getstat` hit the driver's untracked statistics counters,
    /// an incoming datagram is never delivered, and `gen_tcp:connect/3` refuses), the
    /// `prim_inet` module whose calls would drive them, and the port-monitor term (see the
    /// per-item comments below).
    ///
    /// `port_command/2` is pinnable **only on a closed port** (both runtimes answer `badarg`):
    /// on the socket-less port the two runtimes share no observable at all (the oracle answers
    /// `true` and then kills the port, `beam_rt` raises `badarg` and keeps it — see the module
    /// docs), so a `port_command` row must close the port first.
    #[test]
    fn no_case_pins_a_measured_divergence() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for forbidden in [
                // A send (and the statistics query) reaches the driver's untracked counters, so
                // it is a typed `unsupported` (exit 3) rather than an observable.
                "gen_udp:send",
                "inet:getstat",
                // The read path: no datagram is ever delivered, so `recv` fails inside
                // `prim_inet` (`{case_clause, {ok, []}}`) instead of answering.
                "gen_udp:recv",
                "prim_inet",
                // A stream connect against a live listener is `{ok, Socket}` on the oracle and a
                // `badarg` raise here.
                "gen_tcp:",
                // A port monitor still disagrees on the delivered term: the oracle hands the
                // trapping evaluator `{'EXIT', P, normal}` while `beam_rt` hands it
                // `{'DOWN', Ref, port, P, normal}`.
                "monitor(port",
            ] {
                assert!(
                    !expr.contains(forbidden),
                    "case {} pins `{forbidden}`, a measured divergence (see the module docs)",
                    case.id
                );
            }
            if let Some(sent) = expr.find("port_command") {
                assert!(
                    expr.find("erlang:port_close(P)")
                        .is_some_and(|closed| closed < sent),
                    "case {} sends a port_command before closing the port — the socket-less \
                     shape has no shared observable on the two runtimes (see the module docs)",
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
}
