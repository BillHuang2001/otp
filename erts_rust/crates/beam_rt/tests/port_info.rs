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
//! `erlang:port_info/1,2` completeness, cross-checked against the system oracle (contract A.4).
//!
//! Every probe is one Erlang expression run **twice** — under the system `erl`
//! ([`support::oracle_eval`]: `erl -noshell -eval <expr> -s init stop`) and under this crate's
//! own binary (`beam_rt -noshell -eval <expr>`) — and the trimmed **stdout** of the two runs must
//! be byte-identical, with exit status `0` on both sides. Each expression ends with `halt(0).`, so
//! neither runtime can hang on the probe itself.
//!
//! # What is being closed here
//!
//! The derived `/2`-only items — `queue_size`, `memory`, `monitored_by`, `locking` and
//! `parallelism` — were `error:badarg` before this suite
//! ([`crate::port::info_extras`] carries each one's derivation from the reference C); and
//! `port_info/1` must stay exactly the C's `ERTS_PORT_INFO_1_ITEMS` roster
//! (`erts/emulator/beam/erl_port.h:96-105`: `registered_name` first, dropped when unregistered,
//! then `name`, `links`, `id`, `connected`, `input`, `output`, `os_pid`) — no `/2`-only item may
//! leak into it.
//!
//! # Never compare a raw pid, port number or port id
//!
//! The two runtimes have different pids and different port ids, so every probe normalizes:
//! the `S` masker maps a pid to the atom `pid`, a port to `port` (recursing through lists and
//! tuples), and an `id` item is compared through `is_integer/1` instead of its number.
//!
//! # Skip policy
//!
//! No `erl` on `$PATH` (and no executable `$ERTS_ORACLE`) is a **loud** skip per test, never a
//! silent pass; a probe the oracle cannot answer prints its own skip line and is not compared.
//!
//! # The one deliberate divergence
//!
//! `memory`'s `port_set_data/2` contribution counts the C record's **header** only, not the
//! stored term's words (see [`crate::port::info_extras`]), so `port_set_data(P,{foo,1})` reports
//! `120` here where the oracle reports `136`. No probe below sets a data term, precisely so the
//! measured `memory` figures can be compared exactly.
#![deny(unsafe_code)]
mod support;
use std::path::Path;
/// The runtime binary under test (cargo builds it for this suite).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
// ---------------------------------------------------------------------------------------
// The probes (each one is the literal that both runtimes execute)
// ---------------------------------------------------------------------------------------
/// Probe 1 — every `port_info/2` item of a fresh `{spawn_driver, "ram_file_drv"}` port.
///
/// This is the item vocabulary's whole `/2` surface: the five derived items (`queue_size`,
/// `memory`, `monitored_by`, `locking`, `parallelism`) plus the seven `/1` items, the `/2`-only
/// `monitors`, and the bare `[]` an unregistered port reports for `registered_name`.
///
/// Oracle answer (measured, OTP 29.1.1 / erts-17.1):
/// ```text
/// [{queue_size,0},
///  {memory,48},
///  {monitored_by,[]},
///  {locking,driver_level},
///  {parallelism,false},
///  {name,"ram_file_drv"},
///  {links,[pid]},
///  {id,true},
///  {connected,pid},
///  {input,0},
///  {output,0},
///  {os_pid,undefined},
///  {monitors,[]},
///  []]
/// ```
///
/// `memory` is `48` — one `ErtsLink` node, the owner's (`sizeof(ErtsLink)`) — and `locking` is
/// `driver_level` because `ram_file_drv` does not set `ERL_DRV_FLAG_USE_PORT_LOCKING`.
const PROBE_ITEMS: &str = r#"P = open_port({spawn_driver,"ram_file_drv"},[]),
S = fun F(V) when is_pid(V) -> pid; F(V) when is_port(V) -> port; F(V) when is_list(V) -> [F(X) || X <- V]; F(V) when is_tuple(V) -> list_to_tuple([F(X) || X <- tuple_to_list(V)]); F(V) -> V end,
N = fun(undefined, _) -> undefined; ([], _) -> []; ({K,V}, I) -> {K, case I of id -> is_integer(V); _ -> S(V) end} end,
Items = [queue_size,memory,monitored_by,locking,parallelism,name,links,id,connected,input,output,os_pid,monitors,registered_name],
io:format("~p~n",[[N(erlang:port_info(P,I),I) || I <- Items]]),
halt(0).
"#;
/// Probe 2 — the whole `port_info/1` list: a bare port, then the same port registered, then the
/// list reached through the registered **name**.
///
/// Oracle answer (measured):
/// ```text
/// bare=[{name,"ram_file_drv"},{links,[pid]},{id,true},{connected,pid},
///       {input,0},{output,0},{os_pid,undefined}]
/// reg=[{registered_name,myp},{name,"ram_file_drv"},{links,[pid]},{id,true},{connected,pid},
///      {input,0},{output,0},{os_pid,undefined}]
/// regatom=<the same list>
/// ```
///
/// The roster is load-bearing in both directions: `registered_name` is **first** when the port
/// has a name (the C's `ERTS_PORT_INFO_1_ITEMS` starts with it and drops it when it is
/// `undefined`, `io.c:4802-4824`), and no `/2`-only item (`monitors`, `queue_size`, `memory`,
/// `monitored_by`, `locking`, `parallelism`) appears at all.
const PROBE_INFO_1: &str = r#"P = open_port({spawn_driver,"ram_file_drv"},[]),
S = fun F(V) when is_pid(V) -> pid; F(V) when is_port(V) -> port; F(V) when is_list(V) -> [F(X) || X <- V]; F(V) when is_tuple(V) -> list_to_tuple([F(X) || X <- tuple_to_list(V)]); F(V) -> V end,
L = fun(W) -> [{element(1,T), case element(1,T) of id -> is_integer(element(2,T)); _ -> S(element(2,T)) end} || T <- W] end,
io:format("bare=~p~n",[L(erlang:port_info(P))]),
true = erlang:register(myp,P),
io:format("reg=~p~n",[L(erlang:port_info(P))]),
io:format("regatom=~p~n",[L(erlang:port_info(myp))]),
halt(0).
"#;
/// Probe 3 — an OS-child port (`{spawn, "cat"}`): the same derived items, with `locking` at
/// `port_level` because the C's `spawn` driver **does** set `ERL_DRV_FLAG_USE_PORT_LOCKING`
/// (`erts/emulator/sys/unix/sys_drivers.c:251`).
///
/// Oracle answer (measured):
/// ```text
/// cat=[{queue_size,0},{memory,48},{locking,port_level},{parallelism,false}]
/// cat1=[name,links,id,connected,input,output,os_pid]
/// ```
const PROBE_OS_CHILD: &str = r#"C = open_port({spawn,"cat"},[]),
io:format("cat=~p~n",[[erlang:port_info(C,queue_size), erlang:port_info(C,memory), erlang:port_info(C,locking), erlang:port_info(C,parallelism)]]),
io:format("cat1=~p~n",[[element(1,T) || T <- erlang:port_info(C)]]),
halt(0).
"#;
/// Probe 4 — the `memory` accounting that is **not** a constant: the line buffer and the monitor
/// records (and the link tree, which stays one node however many links there are).
///
/// Oracle answer (measured):
/// ```text
/// line=[{memory,90},{memory,180},{memory,180}]
/// one_watcher=[{memory,72},{monitored_by,1}]
/// three_watchers=[{memory,72},{monitored_by,3}]
/// demonitored=[{memory,48},{monitored_by,[]}]
/// two_links=[{memory,48},{links,2},{connected,true}]
/// ```
///
/// Each number is the C's: a `{line, N}` port adds `sizeof(LineBuf) + min(N, 100)` (`90` for
/// `{line,10}`, `180` for `{line,200}` and for a bare `line`, whose `bufsiz` is 512); a watched
/// port adds the process monitor's half (`72`) — **assigning**, so N watchers still report one
/// record, exactly like the C's `monitor_size` callback — while a second link adds nothing,
/// because `link_size` assigns too.
const PROBE_MEMORY_ACCOUNTING: &str = r#"G = fun(P, I) -> element(2,erlang:port_info(P,I)) end,
L10 = open_port({spawn_driver,"ram_file_drv"},[{line,10}]),
L200 = open_port({spawn_driver,"ram_file_drv"},[{line,200}]),
LBare = open_port({spawn_driver,"ram_file_drv"},[line]),
io:format("line=~p~n",[[{memory,G(L10,memory)}, {memory,G(L200,memory)}, {memory,G(LBare,memory)}]]),
W = open_port({spawn_driver,"ram_file_drv"},[]),
R1 = erlang:monitor(port,W),
io:format("one_watcher=~p~n",[[{memory,G(W,memory)}, {monitored_by,length(G(W,monitored_by))}]]),
R2 = erlang:monitor(port,W),
R3 = erlang:monitor(port,W),
io:format("three_watchers=~p~n",[[{memory,G(W,memory)}, {monitored_by,length(G(W,monitored_by))}]]),
erlang:demonitor(R1,[flush]), erlang:demonitor(R2,[flush]), erlang:demonitor(R3,[flush]),
io:format("demonitored=~p~n",[[{memory,G(W,memory)}, {monitored_by,G(W,monitored_by)}]]),
X = open_port({spawn_driver,"ram_file_drv"},[]),
H = spawn(fun() -> receive stop -> ok end end),
true = erlang:port_connect(X,H),
io:format("two_links=~p~n",[[{memory,G(X,memory)}, {links,length(G(X,links))}, {connected,G(X,connected) =:= H}]]),
H ! stop,
halt(0).
"#;
/// Probe 5 — the `badarg` / `undefined` matrix, `F/1` rendering a caught raise as its **bare
/// reason atom** (so each line is class-independent), and the `undefined` answers printed raw.
///
/// Oracle answer (measured, thirteen lines):
/// ```text
/// unknown_item=badarg       unknown_item2=badarg
/// pid_run=badarg            pid_mem=badarg
/// int_mon=badarg            int_lock=badarg
/// closed1=undefined         closed_mem=undefined
/// closed_qs=undefined       closed_lock=undefined
/// noname1=undefined         noname_lock=undefined
/// procname=undefined
/// ```
///
/// i.e. an unknown **item** on a live port is `badarg` (for the new items too: `stream` and a
/// made-up atom alike), a non-port argument is `badarg` before any item is read, a **closed**
/// port is `undefined` for both arities whatever the item, and an atom naming no port — including
/// one naming a *process* (`init`) — is `undefined` too.
const PROBE_BADARG_MATRIX: &str = r#"F = fun(G) -> case catch G() of {'EXIT',{R,_}} when is_atom(R) -> R; {'EXIT',R} when is_atom(R) -> R; V -> {ok,V} end end,
P = open_port({spawn_driver,"ram_file_drv"},[]),
io:format("unknown_item=~p~n",[F(fun() -> erlang:port_info(P,stream) end)]),
io:format("unknown_item2=~p~n",[F(fun() -> erlang:port_info(P,nosuchitem) end)]),
io:format("pid_run=~p~n",[F(fun() -> erlang:port_info(self(),queue_size) end)]),
io:format("pid_mem=~p~n",[F(fun() -> erlang:port_info(self(),memory) end)]),
io:format("int_mon=~p~n",[F(fun() -> erlang:port_info(3,monitored_by) end)]),
io:format("int_lock=~p~n",[F(fun() -> erlang:port_info(3,locking) end)]),
Closed = open_port({spawn_driver,"ram_file_drv"},[]),
true = erlang:port_close(Closed),
io:format("closed1=~p~n",[erlang:port_info(Closed)]),
io:format("closed_mem=~p~n",[erlang:port_info(Closed,memory)]),
io:format("closed_qs=~p~n",[erlang:port_info(Closed,queue_size)]),
io:format("closed_lock=~p~n",[erlang:port_info(Closed,locking)]),
io:format("noname1=~p~n",[erlang:port_info(nosuchname)]),
io:format("noname_lock=~p~n",[erlang:port_info(nosuchname,locking)]),
io:format("procname=~p~n",[erlang:port_info(init)]),
halt(0).
"#;
// ---------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------
/// Runs `expr` under the oracle and under [`BINARY`] and returns `(oracle_stdout, runtime_stdout)`,
/// both trimmed.
///
/// `None` — after a **loud** skip line — only when the *oracle* cannot answer (a missing
/// toolchain, a probe that timed out, a non-zero exit). A runtime that cannot answer is a genuine
/// divergence and fails the test with its stderr.
fn side_by_side(oracle: &Path, expr: &str) -> Option<(String, String)> {
    let Some(expected) = support::oracle_eval(oracle, None, expr) else {
        eprintln!(
            "SKIP the port_info probe: the oracle could not answer\n--- expression ---\n{expr}"
        );
        return None;
    };
    let output = support::run_bounded(Path::new(BINARY), &["-noshell", "-eval", expr])
        .expect("beam_rt answers the probe within the bound");
    assert!(
        output.status.success(),
        "beam_rt must exit 0 for this expression, got {:?}\n--- stderr ---\n{}\n--- expression ---\n{expr}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let ours = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some((expected, ours))
}
/// Asserts that `expr` prints **exactly** the oracle's bytes on this runtime.
fn agrees(oracle: &Path, expr: &str) {
    let Some((expected, ours)) = side_by_side(oracle, expr) else {
        return;
    };
    assert_eq!(
        ours, expected,
        "the two runtimes must print the same answer\n--- expression ---\n{expr}"
    );
}
// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------
/// Probe 1: every `/2` item, including the five derived ones, on a bare driver port.
#[test]
fn every_port_info_2_item_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("port_info") else {
        return;
    };
    agrees(&oracle, PROBE_ITEMS);
}
/// Probe 2: `port_info/1` is the C's roster — `registered_name` first when named, nothing
/// `/2`-only ever.
#[test]
fn the_port_info_1_roster_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("port_info") else {
        return;
    };
    agrees(&oracle, PROBE_INFO_1);
}
/// Probe 3: an OS-child port's `queue_size`/`memory`/`locking`/`parallelism` (`port_level`).
#[test]
fn an_os_child_port_reports_the_same_items() {
    let Some(oracle) = support::require_oracle("port_info") else {
        return;
    };
    agrees(&oracle, PROBE_OS_CHILD);
}
/// Probe 4: the `memory` accounting — line buffer, monitor records, link nodes.
#[test]
fn the_memory_accounting_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("port_info") else {
        return;
    };
    agrees(&oracle, PROBE_MEMORY_ACCOUNTING);
}
/// Probe 5: the `badarg` / `undefined` matrix for both arities.
#[test]
fn the_badarg_and_undefined_matrix_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("port_info") else {
        return;
    };
    agrees(&oracle, PROBE_BADARG_MATRIX);
}
