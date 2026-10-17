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
//! Contract **A.4** — the `udp_inet` built-in driver's observable port surface.
//!
//! `beam_rt`'s built-in driver registry serves `udp_inet`
//! (`../../crates/beam_io/src/drivers/inet/udp.rs`), so `open_port({spawn_driver,
//! "udp_inet"}, Opts)` must resolve the name to a **real emulator port** exactly as the
//! oracle's statically linked `udp_inet_driver_entry` (`inet_drv.c:1406`) does. What this group
//! pins is the port surface that resolution produces, driven only through the documented
//! `erlang` port BIFs — the program comes from the CLI (contract A.6), the port itself is A.4:
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
//!   local integer);
//! * **the close lifecycle** — `port_close/1` answers `true`, the port leaves `ports/0`, the
//!   `port_info` view becomes `undefined` (both `port_info/1` and every item), a second close is
//!   `error:badarg`, and the term is still a port (`is_port/1`), just not a pid;
//! * **port-term identity** — `list_to_port(port_to_list(P))` is `P`;
//! * **one name per driver** — opening `"udp_inet"` and `"tcp_inet"` in the same run reports
//!   `udp_inet` and `tcp_inet` respectively (a name is never aliased or shared);
//! * **the emulator accounting** — `erlang:system_info(port_count)` grows by exactly one while
//!   the port is open and is back where it started after the close.
//!
//! # Determinism (no socket, no clock, no pid is pinned)
//!
//! `open_port/2` on a `{spawn_driver, Name}` port only builds the driver's descriptor: the
//! oracle's C creates the socket in `INET_REQ_OPEN` (`packet_inet_ctl`, `inet_drv.c:14577`) and
//! this floor's does the same in its `open` verb, so **no case here opens or binds a socket** —
//! there is nothing to race and no network (not even loopback) is touched. Every case closes the
//! ports it opened, so no case leaks a port into the next one. Nothing volatile is asserted: the
//! port term itself, the `port_info(P, id)` index, the owning pid and the `ports/0` length are
//! all specific to one VM, so they are compared **relative** to `self()`/before-open (`=:=
//! self()`, `length(Links)`, `During - Before`) or only by their class (`is_integer/1`,
//! `is_port/1`) — never by value. Every expression terminates on its own with no `receive`, so
//! there is no `after` guard to need (and none is missing).
//!
//! # Why the datagram half is deliberately absent (measured, never allowlisted)
//!
//! The driver's control vocabulary (`open`/`bind`/`connect`/`send`/`recv`/`setopts`/`getopts` —
//! `../../crates/beam_io/src/drivers/inet/udp.rs`) is **unreachable from Erlang on one of the
//! two runtimes**, so no datagram round trip, option reply or `{error, …}` refusal can be
//! pinned without committing a `mismatch`. Measured in this environment (oracle OTP 29.1.1 /
//! `erts-17.1`, and the built `beam_rt`):
//!
//! * `erlang:port_control(P, 1 + 16#03f1a300, <<1,2,0>>)` (the oracle's `INET_REQ_OPEN` through
//!   `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`, `prim_inet.erl:2969`) — the oracle answers `[1]`;
//!   `beam_rt` raises `error:badarg`. `beam_rt`'s `erlang:port_control/3` forwards the
//!   operation **verbatim** as its decimal string (`runtime-native` port floor), which is a C
//!   driver's convention, while the built-in inet drivers' `Driver::control` speaks *named*
//!   verbs — so no `INET_REQ_*`/`PACKET_REQ_*` code can reach `udp_inet`. Owner: `beam_rt`
//!   (`port_control` verb bridge) / `beam_io`.
//! * `erlang:port_command(P, <<"hi">>)` on the socket-less port — the oracle raises
//!   `error:badarg` (the C's `!IS_OPEN` guard, `packet_inet_command`, `inet_drv.c:15051`), while
//!   `beam_rt` answers `true` and leaves the bytes queued. Owner: `beam_rt` / `beam_io`.
//! * `gen_udp:open(0, [])` — the oracle answers `{ok, #Port<…>}`; `beam_rt` answers
//!   `{error, einval}` (the same missing verb bridge: `prim_inet` drives the driver through
//!   those codes). Owner: `beam_rt` / `beam_io`.
//! * `erlang:port_info(P, queue_size)` (`{queue_size,0}`) and `erlang:port_info(P, memory)`
//!   (`{memory,48}`) — the oracle answers the item, `beam_rt` raises `error:badarg`. Owner:
//!   `beam_rt` (the native `port_info/2` floor).
//! * `erlang:port_connect(P, self())` — the oracle answers `true`, `beam_rt` reports the typed
//!   capability gap `beam_rt: unsupported: erlang:port_connect/2 is a known BIF that is not
//!   implemented`. Owner: `beam_bifs` / `beam_rt`.
//! * `erlang:monitor(port, P)` + `port_close/1` — the oracle's owner (the evaluator process,
//!   which traps exits) receives `{'EXIT', P, normal}`; `beam_rt` delivers nothing. Owner:
//!   `beam_rt` (port exit signalling).
//!
//! The group therefore pins the whole port surface the driver *does* agree on, and nothing it
//! does not.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.4 `udp_inet` eval case.
///
/// [`Probe::Value`] prints the expression's own answer as a readable line (`ok {name, …}`,
/// `ok badarg`), and the golden is the **oracle's** exact stdout line — measured with
/// `differential run <id>`, never hand-written. Every case exits `0`: the one refusal shape
/// (`error:badarg` on a second `port_close/1`) is caught *inside* the expression, so the
/// observable is the reason atom rather than an uncaught exception.
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
        assert_eq!(cases.len(), 19, "the group's size is pinned by this test");
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

    /// The shapes that were **measured** to diverge stay out of the group: the driver's
    /// `INET_REQ_*`/`PACKET_REQ_*` verbs are unreachable from Erlang on `beam_rt`
    /// (`port_control` forwards the operation as a decimal *verb string* that a built-in driver
    /// never matches), so every datagram/option shape — and `port_command/2` on the socket-less
    /// port, and the two `port_info/2` items the native floor does not serve — would be a
    /// committed `mismatch`. See the module docs for the exact pairs of observations.
    #[test]
    fn no_case_pins_a_measured_divergence() {
        for case in group() {
            let expr = case.expr().expect("eval case");
            for forbidden in [
                "port_control",
                "port_command",
                "gen_udp",
                "port_connect",
                "queue_size",
                "memory",
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
}
