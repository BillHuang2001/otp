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
//! Contract **A.4** — the `ram_file_drv` built-in driver's observable port surface, reply
//! bytes included.
//!
//! `beam_rt`'s built-in driver registry serves `ram_file_drv`
//! (`../../crates/beam_io/src/drivers/ram_file.rs`, the driver behind `ram_file:*`), so the
//! driver is reachable through the documented `erlang:open_port/2` surface: the port is opened
//! on the driver, reports the driver's name through `port_info/1,2`, accepts a `port_command/2,3`
//! payload for every command word the driver defines, counts that payload in its `output`
//! counter, and is torn down by `port_close/1`. Nothing here inspects the driver internally —
//! every observable is a documented port BIF reply compared as **stdout + exit status**
//! through the documented CLI (contract A.6).
//!
//! # What these cases pin
//!
//! A `ram_file` port is **command driven**: it owns no file descriptor, and every operation's
//! reply (`NUMBER`/`DATA`/`OK`/`ERROR <errno>`) is written back from the driver's `output`
//! callback as a `{Port, {data, Bytes}}` message. Those replies **do reach the port owner on
//! both runtimes**, so this group pins both halves of the surface:
//!
//! * the **port-observable** half — the registered name (also under the C's "name up to the
//!   first blank" resolution rule), the absence of an OS child, the owner connection, the byte
//!   counters (`output` counting the command payload that reached the port, `input` counting
//!   the reply bytes that came back), `erlang:port_set_data/2`/`port_get_data/1`, the
//!   acceptance of every opcode payload, and the close/teardown refusals;
//! * the **reply** half — an `open` reply really arriving as a `{Port, {data, …}}` message, the
//!   whole `open`/`write`/`lseek`/`read`/`truncate`/`size`/`get` round trip pinned by its exact
//!   reply bytes, and the `NUMBER`/`OK`/`ERROR` reply families arriving as a **proper list**
//!   (never a binary), even on a port opened in `[binary]` stream mode;
//! * the **`control` refusal** — the C driver's `control` slot is `NULL`, so
//!   `erlang:port_control/3` raises `badarg` for every opcode on both runtimes.
//!
//! Every row `agree`s and self-terminates: an operation is a synchronous `port_command/2` call
//! (or a plain BIF) followed by one guarded `receive`, so no row waits on an asynchronous reply
//! it did not itself trigger, and every port is closed before the expression answers.
//!
//! # Reply payload projection (why a `DATA` reply is pinned as bytes)
//!
//! The driver writes a reply in two pieces (`driver_output2`: the 5-byte `DATA` header, then the
//! body), so the oracle hands a `DATA` reply's payload to the owner as the *improper* list
//! `[2,0,0,0,n | Body]` while the runtime flattens it into the equivalent proper list. Both are
//! the same byte sequence and `iolist_to_binary/1` normalizes either, so the round-trip case pins
//! `iolist_to_binary/1` of each reply — byte-exactly — and never the improper/proper list shape.
//! `NUMBER`/`OK`/`ERROR` replies carry no body at all and are a proper list on both runtimes, so
//! their payload shape is pinned directly.
//!
//! Ids are `ram_file/<snake_case_name>`; the contract is A.4 ([`Contract::DriverAbi`]) — the
//! port BIF replies these rows read (`port_info/1,2`, `port_command/2,3`, `port_control/3`,
//! `port_set_data/2`, `port_get_data/1`, `port_close/1`) are the A.4 delivery mechanism itself.
//! Every golden is the **oracle's** own output, measured with `differential compare` / `run`
//! (never a hand-written guess).
//!
//! The structural half of A.4 (that a C translation unit referencing the `driver_*` /
//! `erl_drv_*` declarations still compiles against the frozen headers) is asserted in
//! `tests/abi.rs`.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `ram_file/*` eval case.
///
/// The expression is a `begin … end` block that opens the port, collects the terms it pins,
/// closes the port and answers the collected term, so a case never leaves a port behind. Every
/// refusal is projected to its `badarg` *reason atom* (`case catch … of {'EXIT',{badarg,_}} ->
/// badarg`) — never a stack trace — and `Probe::Value` prints the whole answer as one readable
/// line (`ok {true,{data,[3,0,0,0,0]}}`).
fn case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::DriverAbi],
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

/// The `ram_file/*` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "ram_file/port_reports_the_driver_name",
            "a binary-mode and a list-mode port both report {name,\"ram_file_drv\"} (the driver is the same in either stream mode)",
            "begin P1 = open_port({spawn_driver,\"ram_file_drv\"},[binary]), P2 = open_port({spawn_driver,\"ram_file_drv\"},[]), Acc = {erlang:is_port(P1), erlang:list_to_atom(element(2,erlang:port_info(P1,name))), erlang:list_to_atom(element(2,erlang:port_info(P2,name)))}, erlang:port_close(P1), erlang:port_close(P2), Acc end",
            "ok {true,ram_file_drv,ram_file_drv}",
        ),
        case(
            "ram_file/port_name_is_the_full_spawn_driver_string",
            "port_info/2 reports the whole spawn_driver string, so a blank-suffixed name still opens the driver and still accepts a command",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv extra\"},[binary]), Acc = {erlang:list_to_atom(element(2,erlang:port_info(Pt,name))), erlang:port_command(Pt, <<1,0,0,0,0>>)}, erlang:port_close(Pt), Acc end",
            "ok {'ram_file_drv extra',true}",
        ),
        case(
            "ram_file/port_has_no_os_process",
            "the driver port has no OS child: port_info(Port, os_pid) is {os_pid,undefined}",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), Acc = erlang:port_info(Pt,os_pid), erlang:port_close(Pt), Acc end",
            "ok {os_pid,undefined}",
        ),
        case(
            "ram_file/port_is_connected_to_a_process",
            "port_info(Port, connected) names the owning process (asserted as its pid shape, never the volatile pid)",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), Acc = erlang:is_pid(element(2,erlang:port_info(Pt,connected))), erlang:port_close(Pt), Acc end",
            "ok true",
        ),
        case(
            "ram_file/fresh_port_counters_are_zero",
            "a freshly opened driver port reports zero input and output bytes",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), Acc = {erlang:port_info(Pt,input), erlang:port_info(Pt,output)}, erlang:port_close(Pt), Acc end",
            "ok {{input,0},{output,0}}",
        ),
        case(
            "ram_file/output_counter_counts_the_command_payload_bytes",
            "the port's output counter grows by exactly the command payload: an 8-byte open, then a 3-byte write, read back as {8,11}",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), erlang:port_command(Pt, <<1,0,0,0,3,97,98,99>>), A1 = element(2, erlang:port_info(Pt,output)), erlang:port_command(Pt, <<4,120,121>>), A2 = element(2, erlang:port_info(Pt,output)), erlang:port_close(Pt), {A1,A2} end",
            "ok {8,11}",
        ),
        case(
            "ram_file/port_command_accepts_every_opcode_payload",
            "port_command/2 accepts a payload for every command word the driver defines (open/read/lseek/write/fsync/truncate/pread/pwrite/fdatasync/get/size/advise/allocate) and answers true for each",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), Cmds = [<<1,0,0,0,3,97,98,99>>, <<2,0,0,0,2>>, <<3,0,0,0,0,0,0,0,0>>, <<4,120,121>>, <<9>>, <<14>>, <<17,0,0,0,1,0,0,0,2>>, <<18,0,0,0,0,122>>, <<19>>, <<30>>, <<37>>, <<38,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0>>, <<39,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0>>], Acc = lists:usort([erlang:port_command(Pt, Cmd) || Cmd <- Cmds]), erlang:port_close(Pt), Acc end",
            "ok [true]",
        ),
        case(
            "ram_file/port_close_is_true_and_a_second_close_is_badarg",
            "port_close/1 answers true once and the second close of the same driver port is badarg",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), First = erlang:port_close(Pt), Second = case catch erlang:port_close(Pt) of {'EXIT',{badarg,_}} -> badarg; Other -> Other end, {First,Second} end",
            "ok {true,badarg}",
        ),
        case(
            "ram_file/port_info_after_close_is_undefined",
            "port_info/1 of a closed driver port is the atom undefined",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), erlang:port_close(Pt), erlang:port_info(Pt) end",
            "ok undefined",
        ),
        case(
            "ram_file/ports_lists_the_port_while_it_is_open",
            "erlang:ports/0 lists the driver port while it is open and no longer after port_close/1",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), Before = lists:member(Pt, erlang:ports()), erlang:port_close(Pt), {Before, lists:member(Pt, erlang:ports())} end",
            "ok {true,false}",
        ),
        case(
            "ram_file/port_command_after_close_is_badarg",
            "port_command/2 to a closed driver port is badarg (the driver is gone, never a silent no-op)",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), erlang:port_close(Pt), case catch erlang:port_command(Pt, <<1,0,0,0,0>>) of {'EXIT',{badarg,_}} -> badarg; Other -> Other end end",
            "ok badarg",
        ),
        case(
            "ram_file/reply_bytes_reach_the_owner",
            "the driver's reply reaches the port owner as a {Port,{data,Payload}} message: an open command answers the NUMBER 0 reply [3,0,0,0,0]",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), erlang:port_command(Pt, <<1,0,0,0,3,97,98,99>>), Msg = receive M -> M after 5000 -> timeout end, Acc = {element(1,Msg) =:= Pt, element(2,Msg)}, erlang:port_close(Pt), Acc end",
            "ok {true,{data,[3,0,0,0,0]}}",
        ),
        case(
            "ram_file/input_counter_counts_the_reply_bytes",
            "after an 8-byte open command and its 5-byte reply, the port reports {input,5} and {output,8}",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), erlang:port_command(Pt, <<1,0,0,0,3,97,98,99>>), _ = receive {Pt,{data,_}} -> ok after 5000 -> timeout end, Acc = {erlang:port_info(Pt,input), erlang:port_info(Pt,output)}, erlang:port_close(Pt), Acc end",
            "ok {{input,5},{output,8}}",
        ),
        case(
            "ram_file/read_write_seek_truncate_replies",
            "the whole efile reply matrix round trips: open (NUMBER 0), write abc (NUMBER 3), lseek 1 (NUMBER 1), read 2 (DATA \"bc\"), lseek 1, truncate (OK), size (NUMBER 1), get (DATA \"a\") — pinned as the exact reply bytes",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), port_command(Pt, <<1,0,0,0,3>>), R1 = receive {Pt,{data,D1}} -> D1 after 5000 -> timeout end, port_command(Pt, <<4,97,98,99>>), R2 = receive {Pt,{data,D2}} -> D2 after 5000 -> timeout end, port_command(Pt, <<3,0,0,0,1,0,0,0,0>>), R3 = receive {Pt,{data,D3}} -> D3 after 5000 -> timeout end, port_command(Pt, <<2,0,0,0,2>>), R4 = receive {Pt,{data,D4}} -> D4 after 5000 -> timeout end, port_command(Pt, <<3,0,0,0,1,0,0,0,0>>), R5 = receive {Pt,{data,D5}} -> D5 after 5000 -> timeout end, port_command(Pt, <<14>>), R6 = receive {Pt,{data,D6}} -> D6 after 5000 -> timeout end, port_command(Pt, <<37>>), R7 = receive {Pt,{data,D7}} -> D7 after 5000 -> timeout end, port_command(Pt, <<30>>), R8 = receive {Pt,{data,D8}} -> D8 after 5000 -> timeout end, erlang:port_close(Pt), [iolist_to_binary(X) || X <- [R1,R2,R3,R4,R5,R6,R7,R8]] end",
            "ok [<<3,0,0,0,0>>,<<3,0,0,0,3>>,<<3,0,0,0,1>>,<<2,0,0,0,2,98,99>>,<<3,0,0,0,1>>,<<0>>,<<3,0,0,0,1>>,<<2,0,0,0,1,97>>]",
        ),
        case(
            "ram_file/port_control_is_badarg_for_every_verb",
            "the C driver's control slot is NULL, so erlang:port_control/3 raises badarg for every opcode — with an empty payload and with a real one",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), port_command(Pt, <<1,0,0,0,3,97,98,99>>), _ = receive {Pt,{data,_}} -> ok after 5000 -> ok end, Ops = [0,1,2,3,4,9,14,17,18,19,30,37,38,39], Reasons = lists:usort([case catch erlang:port_control(Pt, Op, Arg) of {'EXIT',{badarg,_}} -> badarg; Other -> Other end || {Op,Arg} <- [{O,<<>>} || O <- Ops] ++ [{37,<<1,2,3>>},{1,<<1,0,0,0,0>>}]]), erlang:port_close(Pt), Reasons end",
            "ok [badarg]",
        ),
        case(
            "ram_file/port_set_data_and_get_data_round_trip",
            "erlang:port_set_data/2 answers true and port_get_data/1 hands the stored term back (a literal tuple, then the owner's own pid — compared, never printed)",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), S1 = erlang:port_set_data(Pt, {tag,42}), G1 = erlang:port_get_data(Pt), S2 = erlang:port_set_data(Pt, erlang:self()), G2 = erlang:port_get_data(Pt), Acc = {S1,G1,G1 =:= {tag,42},S2,G2 =:= erlang:self()}, erlang:port_close(Pt), Acc end",
            "ok {true,{tag,42},true,true,true}",
        ),
        case(
            "ram_file/binary_stream_number_reply_is_a_proper_list",
            "in [binary] stream mode a NUMBER reply arrives as a proper list of 5 byte values, not a binary",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), port_command(Pt, <<1,0,0,0,0>>), Rep = receive {Pt,{data,D}} -> D after 5000 -> timeout end, Acc = {erlang:is_list(Rep), length(Rep), lists:all(fun erlang:is_integer/1, Rep)}, erlang:port_close(Pt), Acc end",
            "ok {true,5,true}",
        ),
        case(
            "ram_file/binary_stream_reply_is_not_a_binary",
            "even on a port opened in [binary] mode the reply payload is a list, never a binary (its head is the NUMBER response code 3)",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), port_command(Pt, <<1,0,0,0,0>>), Rep = receive {Pt,{data,D}} -> D after 5000 -> timeout end, Acc = {erlang:is_binary(Rep), erlang:is_list(Rep), hd(Rep)}, erlang:port_close(Pt), Acc end",
            "ok {false,true,3}",
        ),
        case(
            "ram_file/binary_stream_ok_reply_is_a_proper_list",
            "an OK reply (fsync on an opened file) arrives as the one-element proper list [0] — a list, not a binary",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), port_command(Pt, <<1,0,0,0,3>>), _ = receive {Pt,{data,_}} -> ok after 5000 -> timeout end, port_command(Pt, <<9>>), Rep = receive {Pt,{data,D}} -> D after 5000 -> timeout end, Acc = {erlang:is_list(Rep), length(Rep), Rep}, erlang:port_close(Pt), Acc end",
            "ok {true,1,[0]}",
        ),
        case(
            "ram_file/binary_stream_error_reply_is_a_proper_list",
            "an ERROR reply (truncate on a read-only file) arrives as a proper list of the ERROR response code 1 plus the lowercased errno name eacces",
            "begin Pt = open_port({spawn_driver,\"ram_file_drv\"},[binary]), port_command(Pt, <<1,0,0,0,1>>), _ = receive {Pt,{data,_}} -> ok after 5000 -> timeout end, port_command(Pt, <<14>>), Rep = receive {Pt,{data,D}} -> D after 5000 -> timeout end, Acc = {erlang:is_list(Rep), length(Rep), Rep}, erlang:port_close(Pt), Acc end",
            "ok {true,7,[1,101,97,99,99,101,115]}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::Contract;

    /// Every row is an A.4 `Probe::Value` eval case whose golden is one measured line.
    #[test]
    fn every_case_is_an_a4_value_case_with_a_measured_one_line_golden() {
        let cases = group();
        assert_eq!(
            cases.len(),
            20,
            "the A.4 ram_file_drv group has twenty cases"
        );
        for case in &cases {
            assert!(case.id.starts_with("ram_file/"), "{}", case.id);
            assert_eq!(case.contracts, &[Contract::DriverAbi], "{}", case.id);
            assert!(!case.description.is_empty(), "{}", case.id);
            assert!(case.extra_argv().is_empty(), "{}", case.id);
            assert!(case.env().is_empty(), "{}", case.id);
            let stdout = case.golden.stdout.expect("every case carries a golden");
            assert!(
                stdout.starts_with("ok "),
                "{}: a Value golden is `ok …`",
                case.id
            );
            assert!(
                !stdout.contains('\n'),
                "{}: the golden is a single line without a trailing newline",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            let expr = case.expr().expect("eval case");
            assert!(
                !expr.contains('\n'),
                "{}: the expression is a single line",
                case.id
            );
            // Every row opens a port, so every row must close it again.
            assert!(
                expr.matches("port_close(").count() >= expr.matches("open_port(").count(),
                "{}: every opened port is closed",
                case.id
            );
            // The probe wrapper binds V/C/R — a case may not shadow them.
            for reserved in [" V ", " C ", " R "] {
                assert!(
                    !expr.contains(reserved),
                    "{}: the expression must not rebind {reserved:?}",
                    case.id
                );
            }
        }
        assert_eq!(
            cases.iter().map(|case| case.id).collect::<Vec<_>>(),
            [
                "ram_file/port_reports_the_driver_name",
                "ram_file/port_name_is_the_full_spawn_driver_string",
                "ram_file/port_has_no_os_process",
                "ram_file/port_is_connected_to_a_process",
                "ram_file/fresh_port_counters_are_zero",
                "ram_file/output_counter_counts_the_command_payload_bytes",
                "ram_file/port_command_accepts_every_opcode_payload",
                "ram_file/port_close_is_true_and_a_second_close_is_badarg",
                "ram_file/port_info_after_close_is_undefined",
                "ram_file/ports_lists_the_port_while_it_is_open",
                "ram_file/port_command_after_close_is_badarg",
                "ram_file/reply_bytes_reach_the_owner",
                "ram_file/input_counter_counts_the_reply_bytes",
                "ram_file/read_write_seek_truncate_replies",
                "ram_file/port_control_is_badarg_for_every_verb",
                "ram_file/port_set_data_and_get_data_round_trip",
                "ram_file/binary_stream_number_reply_is_a_proper_list",
                "ram_file/binary_stream_reply_is_not_a_binary",
                "ram_file/binary_stream_ok_reply_is_a_proper_list",
                "ram_file/binary_stream_error_reply_is_a_proper_list",
            ]
        );
    }

    /// The reply-pinning rows really carry a `{Port, {data, …}}` receive and never a raw
    /// volatile value (a pid/port/ref is always compared, never printed).
    #[test]
    fn the_reply_cases_collect_a_message_and_pin_no_volatile_value() {
        for id in [
            "ram_file/reply_bytes_reach_the_owner",
            "ram_file/input_counter_counts_the_reply_bytes",
            "ram_file/read_write_seek_truncate_replies",
            "ram_file/binary_stream_number_reply_is_a_proper_list",
            "ram_file/binary_stream_reply_is_not_a_binary",
            "ram_file/binary_stream_ok_reply_is_a_proper_list",
            "ram_file/binary_stream_error_reply_is_a_proper_list",
        ] {
            let case = group()
                .into_iter()
                .find(|c| c.id == id)
                .expect("registered");
            let expr = case.expr().expect("eval case");
            assert!(expr.contains("receive"), "{id}");
            assert!(expr.contains("after 5000 ->"), "{id} is bounded");
            assert!(
                !case.golden.stdout.expect("golden").contains("<0."),
                "{id}: a pid/port/ref image must never be pinned"
            );
        }
        // `port_set_data/2` stores the owner's own pid, but only ever *compares* it.
        let case = group()
            .into_iter()
            .find(|c| c.id == "ram_file/port_set_data_and_get_data_round_trip")
            .expect("registered");
        let expr = case.expr().expect("eval case");
        assert!(
            expr.contains("erlang:self()"),
            "the round trip compares self()"
        );
        assert!(
            !case.golden.stdout.expect("golden").contains("<0."),
            "the stored pid is compared, never printed"
        );
    }
}
