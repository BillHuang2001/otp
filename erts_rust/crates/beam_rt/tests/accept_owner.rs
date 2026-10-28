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
//! **An accepted socket belongs to the process that accepted it** (contract A.4), end to end and
//! cross-checked against the live `erl` oracle.
//!
//! # The rule
//!
//! ERTS gives the new `tcp_inet` port the accept's **issuer** as its connected process:
//! `ErlDrvTermData owner = driver_caller(desc->inet.port)`
//! (`../../erts/emulator/drivers/common/inet_drv.c:12232`) is handed to
//! `tcp_inet_copy(desc, s, owner, &err)` (`:12236`), which calls
//! `driver_create_port(port, owner, "tcp_inet", …)` (`:11834`); the queued accept answers the same
//! with the operation's own `desc->inet.opt->caller` (`:13509`). It is **not** the listening
//! socket's owner — accepting on another process's listener is the ordinary acceptor-pool shape
//! (`ranch_acceptor` accepts on the listener `ranch_conns_sup` owns).
//!
//! Two observable facts fall out of it, and both are what the cases below pin:
//!
//! * `erlang:port_info(S, connected)` is `{connected, self()}` in the **acceptor**, which is what
//!   `inet.erl`'s `tcp_controlling_process/2` compares against `self()` before it transfers the
//!   socket — an acceptor that is a stranger to its own socket gets `{error, not_owner}` and
//!   `ranch_acceptor` closes the connection instead of handing it to a handler process;
//! * an `{active, once}` socket delivers `{tcp, S, Data}` to the process the port is connected to
//!   (`PortMeta.owner` is the C's `desc->connected`), so a wrong owner puts the connection's data
//!   in the acceptor pool supervisor's mailbox rather than the handler's.
//!
//! # The rows
//!
//! | Step | What it pins |
//! |---|---|
//! | the acceptor's own `port_info(S, connected)` | the adopted port's **connected** process is the accept's issuer |
//! | `gen_tcp:controlling_process(S, Q)` from the acceptor | `ok`, never `{error, not_owner}` — the transfer is even reachable |
//! | Q's `port_info(S, connected)` after the transfer | the hand-over really moved the owner |
//! | an `{active, once}` delivery after the transfer | `{tcp, S, <<"hi">>}` reaches **Q**, and the listening socket's owner receives nothing |
//!
//! The fixture is compiled with the system `erlc` at test time and run on the **`run`** route — a
//! scheduled process over the real scheduler/poller (`Runtime::run_scheduled`), so a spawned
//! acceptor really blocks in its `receive` and is resumed by the driver's completion. Every wait
//! is deadline-bounded (the run action's own round would otherwise report its honest `blocked`),
//! and the oracle is the system `erl` on the same module (`-s aowner go -s init stop`); a missing
//! toolchain is a loud skip, never a silent pass.

#[allow(dead_code)]
mod support;

/// The fixture: one listening socket owned by the run target, one spawned **acceptor** that does
/// not own it, one third process that receives the socket, one answer line.
///
/// `go/0` owns both the listener and the client end; the acceptor accepts, checks its own
/// ownership, transfers the socket to a third process, and reports the third process's own view
/// plus what the armed socket delivered.
const SOURCE: &str = r#"
-module(aowner).
-export([go/0]).

go() ->
    {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]),
    {ok,Port} = inet:port(L),
    {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),
    Self = self(),
    _ = spawn(fun() -> acceptor(Self, L) end),
    Armed = receive armed -> ok after 8000 -> no_armed end,
    Sent = gen_tcp:send(Cl, <<"hi">>),
    Report = receive {report,R} -> R after 8000 -> no_report end,
    %% The listening socket's owner is *this* process; a delivery here is the ownership bug.
    Leaked = receive {tcp,_,_} -> to_listener_owner after 0 -> none end,
    _ = gen_tcp:close(Cl),
    _ = gen_tcp:close(L),
    io:format("own=~p sent=~p armed=~p leak=~p~n",[Report,Sent,Armed,Leaked]).

acceptor(Go, L) ->
    Me = self(),
    {ok,S} = gen_tcp:accept(L),
    %% The accept's issuer, not the listener's owner.
    Accepted = erlang:port_info(S, connected) =:= {connected, self()},
    Q = spawn(fun() -> third(Me) end),
    Xfer = gen_tcp:controlling_process(S, Q),
    Q ! {socket, S},
    Third = receive {third,TR} -> TR after 8000 -> no_third end,
    Go ! armed,
    Data = receive {third_data,DR} -> DR after 8000 -> no_data end,
    Go ! {report,{Accepted,Xfer,Third,Data}},
    ok.

third(Parent) ->
    receive
        {socket,S} ->
            Mine = erlang:port_info(S, connected) =:= {connected, self()},
            Arm = inet:setopts(S, [{active,once}]),
            Parent ! {third,{Mine,Arm}},
            D = receive {tcp,S,Data} -> {data,Data} after 8000 -> timeout end,
            Parent ! {third_data,D}
    end.
"#;

/// The oracle's and this runtime's common answer: the acceptor owns the socket it accepted, the
/// transfer is accepted, the third process then owns it and receives its data, and the listening
/// socket's owner is handed nothing.
const EXPECTED: &str = "own={true,ok,{true,ok},{data,<<\"hi\">>}} sent=ok armed=ok leak=none\n";

/// Runs `aowner:go/0` on **both** runtimes, asserts the oracle still answers [`EXPECTED`], then
/// pins this runtime's own bytes and exit code.
///
/// The runtime's `run` action prints the target's result (`ok`) after the probe's own line, so
/// the two stdouts differ by exactly that trailing line; anything else is a mismatch.
#[test]
fn an_accepted_socket_belongs_to_the_process_that_accepted_it() {
    const BANNER: &str = "accept_owner";
    const WHAT: &str = "a cross-process accept's ownership";
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "owner", "aowner", SOURCE) else {
        return;
    };
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly(BANNER, "no `erl` oracle on $PATH");
        return;
    };
    let dir = fixture.dir().to_str().expect("the fixture dir is UTF-8");
    let beam = fixture.beam();
    let beam = beam.to_str().expect("the fixture path is UTF-8");
    let Some(oracle_run) = support::run_bounded(
        &oracle,
        &["-noshell", "-pa", dir, "-s", "aowner", "go", "-s", "init", "stop"],
    ) else {
        support::skip_loudly(BANNER, &format!("the oracle did not answer `{WHAT}`"));
        return;
    };
    let oracle_stdout = String::from_utf8_lossy(&oracle_run.stdout).into_owned();
    assert_eq!(
        oracle_stdout, EXPECTED,
        "{WHAT}: the oracle's own answer moved — re-measure before trusting this row"
    );
    let Some(run) = support::run_runtime(&["-noshell", "run", beam, "aowner:go/0"]) else {
        support::skip_loudly(BANNER, &format!("the `run` action did not finish `{WHAT}`"));
        return;
    };
    let wanted = format!("{EXPECTED}ok\n");
    assert_eq!(
        run.stdout, wanted,
        "{WHAT}: the accepted socket's connected process must be the accept's issuer \
         (`driver_caller`, `inet_drv.c:12232`) and not the listener's owner — stdout differs \
         from the oracle's ({oracle_stdout:?}); stderr {:?}",
        run.stderr
    );
    assert_eq!(run.code, 0, "{WHAT}: exit code — stderr {:?}", run.stderr);
    assert!(
        !run.stderr.contains("blocked") && !run.stderr.contains("unsupported"),
        "{WHAT}: every step must complete, not park or decline — stderr {:?}",
        run.stderr
    );
}
