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
//! Contract **A.4** — the `ram_file_drv` built-in driver's observable port surface.
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
//! # What these cases pin, and why no `{Port, {data, …}}` reply is among them
//!
//! A `ram_file` port is **command driven**: it owns no file descriptor, and every operation's
//! reply (`NUMBER`/`DATA`/`OK`/`ERROR <errno>`) is written back from the driver's `output`
//! callback as a `{Port, {data, Bytes}}` message to the port owner. The oracle delivers them;
//! the Rust runtime currently does not (see the divergences below), so the reply **bytes** are
//! not yet a pinnable observable and no case here touches them. What *is* pinnable — and what
//! these rows assert — is the port-observable half of the driver: its registered name (also
//! under the C's "name up to the first blank" resolution rule), the absence of an OS child,
//! its owner connection, the byte counters that prove a command payload reached the port, the
//! acceptance of every opcode payload, and the close/teardown refusals. Every row therefore
//! `agree`s, and every row self-terminates: an operation is a synchronous `port_command/2`
//! call (or a plain BIF), so no row waits on an asynchronous reply it did not itself trigger.
//!
//! Ids are `ram_file/<snake_case_name>`; the contract is A.4 ([`Contract::DriverAbi`]) — the
//! port BIF replies these rows read (`port_info/1,2`, `port_command/2,3`, `port_close/1`) are
//! the A.4 delivery mechanism itself. Every golden is the **oracle's** own output, measured
//! with `differential run <id>` (never a hand-written guess).
//!
//! # Measured divergences that are deliberately NOT cases (handoffs, never `mismatch`-pinned)
//!
//! Each item below is a plain Erlang value/reason, so pinning it would be a `mismatch`; it is
//! reported to the runtime owners instead. Verified in this environment (oracle OTP 29.1.1 /
//! `erts-17.1`; the built `beam_rt`) with `differential compare`:
//!
//! * **the driver's replies never reach the owner (`beam_rt`).** `Pt =
//!   open_port({spawn_driver,"ram_file_drv"},[binary]), port_command(Pt, <<1,0,0,0,3,"abc">>)`
//!   followed by `receive {Pt,{data,D}} -> D after 2000 -> timeout end` yields
//!   `<<3,0,0,0,0>>` (the `open` `NUMBER 0` reply) under the oracle and `timeout` under
//!   `beam_rt`; the same command leaves `port_info(Pt, input)` at `5` on the oracle and `0` on
//!   `beam_rt`. A built-in driver port is recorded as `PortName::SpawnDriverBuiltin`, whose
//!   bytes `PortName::bytes_from_driver_output` excludes from the harvest (they are assumed to
//!   arrive through a poller-registered descriptor, as `tcp_inet`'s do) — but `ram_file_drv`
//!   owns no descriptor, so the reply its `Driver::output` queues is never drained. Every one
//!   of the driver's reply shapes (`open`/`write`/`read`/`lseek`/`truncate`/`fsync`/`pread`/
//!   `pwrite`/`fdatasync`/`get`/`size`/`advise`/`allocate`, and the
//!   `{error, ebadf|eacces|einval}` refusals) waits on that one fix.
//! * **`port_control/3` on the driver (`beam_rt`).** The C driver's `control` slot is `NULL`,
//!   so the oracle raises `badarg` for every operation; after an `open` of `"abc"`,
//!   `port_control(Pt, 37, <<>>)` answers the driver's `NUMBER 3` reply bytes
//!   (`<<3,0,0,0,3>>`) under `beam_rt`, which routes a built-in driver port's `control` to the
//!   Rust driver's named-verb transport.
//! * **`port_info(Pt, queue_size)` (`beam_rt`).** The oracle answers `{queue_size,0}`;
//!   `beam_rt` raises `badarg` (the item is not in its `port_info/2` set).
//! * **the port's `EXIT` signal (`beam_rt`).** With the owner trapping exits,
//!   `port_close(Pt)` delivers `{'EXIT', Pt, normal}` under the oracle; `beam_rt` delivers no
//!   message (the observation is the `after` guard's `none`).
//! * **`erlang:port_set_data/2` (`beam_bifs`/`beam_rt`).** A capability gap: the oracle stores
//!   and returns the term, `beam_rt` reports `unsupported: erlang:port_set_data/2`.
//!
//! The structural half of A.4 (that a C translation unit referencing the `driver_*` /
//! `erl_drv_*` declarations still compiles against the frozen headers) is asserted in
//! `tests/abi.rs`.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `ram_file/*` eval case.
///
/// The expression is a `begin … end` block that opens the port, collects the reply it pins,
/// closes the port and answers the collected term, so a case never leaves a port behind. Every
/// refusal is projected to its `badarg` *reason atom* (`case catch … of {'EXIT',{badarg,_}} ->
/// badarg`) — never a stack trace — and `Probe::Value` prints the whole answer as one readable
/// line (`ok {8,11}`).
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
    ]
}
