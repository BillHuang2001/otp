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

//! The port **BIF** surface, cross-checked against the system oracle (contract A.4).
//!
//! Every probe in this suite is one Erlang expression run **twice** — once under the system
//! `erl` ([`support::oracle_eval`]: `erl -noshell -eval <expr> -s init stop`) and once under this
//! crate's own binary (`beam_rt -noshell -eval <expr>`) — and the trimmed **stdout** of the two
//! runs must be byte-identical, with exit status `0` on both sides. `-eval` prints nothing on
//! success, so every expression prints its own values with `io:format("~p~n", […])` and ends with
//! `halt(0).`; each quoted answer below is what the oracle actually printed on this host
//! (OTP 29.1.1 / erts-17.1), reproduced verbatim by this runtime.
//!
//! The port BIFs are written `erlang:port_set_data/2`, `erlang:port_get_data/1`,
//! `erlang:port_connect/2`, … (fully qualified) for a reason that is itself a measured oracle
//! fact: `erl_eval`'s auto-import list does **not** contain them, so the bare spellings are
//! `undef` under `-eval` (`catch port_get_data(P)` ⇒
//! `{'EXIT',{undef,[{erl_eval,port_get_data,1,[]}, …]}}`) while the qualified ones are served.
//!
//! # What the measured oracle answers are
//!
//! ```text
//! $ erl -noshell -eval 'P = open_port({spawn_driver,"ram_file_drv"},[]),
//!     A = erlang:port_set_data(P,{foo,1}), B = erlang:port_get_data(P), … halt(0).'
//! [true,
//!  {foo,1},
//!  true,true,true,true,
//!  {registered_name,myp},
//!  {registered_name,myp},
//!  false,true,undefined]
//! ```
//!
//! — i.e. the data term round-trips (`{foo,1}`), `port_connect(P, self())` is `true`,
//! `register(myp, P)` is `true`, `whereis(myp)` is a port, `registered()` holds the name,
//! `port_info(P, registered_name)` is `{registered_name, myp}` — which is also the **first** item of
//! the `port_info/1` list, `monitors` is `false` for a port nobody monitors, `port_close(P)` is
//! `true` and the name is gone (`undefined`) afterwards. A fresh port's `port_get_data/1` is
//! `undefined` and its `port_info(P, registered_name)` is the bare empty list `[]`. The badarg
//! matrix (a pid or an integer as the port argument, a **closed** port, a dead pid as the new
//! owner, a port that already has a name, a name a process holds, an unknown name) is `badarg` in
//! all thirteen arms. An owner transfer to a real OS child (`{spawn,"cat"}`) delivers the child's
//! echo to the **new** owner (`transfer={true,<<"hello">>}`), leaves the port with two links and
//! reports `connected` as that helper.
//!
//! # Skip policy
//!
//! No `erl` on `$PATH` (and no executable `$ERTS_ORACLE`) is a **loud** skip per test, never a
//! silent pass; a probe the oracle cannot answer prints its own skip line and is not compared.
#![deny(unsafe_code)]

mod support;

use std::path::Path;

/// The runtime binary under test (cargo builds it for this suite).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The probes (each one is the literal that both runtimes execute)
// ---------------------------------------------------------------------------------------

/// Probe 1 — the whole `{spawn_driver, "ram_file_drv"}` sequence over one port: the opaque data
/// term, the owner re-connect, the name registration, the three name readers and `port_close/1`.
///
/// Oracle answer (one `~p` line per element of the printed list):
/// `[true, {foo,1}, true, true, true, true, {registered_name,myp}, {registered_name,myp}, false,
/// true, undefined]`.
const PROBE_SPAWN_DRIVER: &str = r#"P = open_port({spawn_driver,"ram_file_drv"},[]),
A = erlang:port_set_data(P,{foo,1}),
B = erlang:port_get_data(P),
C = erlang:port_connect(P,self()),
D = erlang:register(myp,P),
E = is_port(erlang:whereis(myp)),
F = lists:member(myp,erlang:registered()),
G = erlang:port_info(P,registered_name),
H = hd(erlang:port_info(P)),
I = lists:keymember(monitors,1,erlang:port_info(P)),
J = erlang:port_close(P),
K = erlang:whereis(myp),
io:format("~p~n",[[A,B,C,D,E,F,G,H,I,J,K]]),
halt(0).
"#;

/// Probe 2 — a fresh, never-touched port: no data term and no name.
///
/// Oracle answer:
/// ```text
/// undefined
/// []
/// ```
const PROBE_FRESH_PORT: &str = r#"P = open_port({spawn_driver,"ram_file_drv"},[]),
io:format("~p~n",[erlang:port_get_data(P)]),
io:format("~p~n",[erlang:port_info(P,registered_name)]),
halt(0).
"#;

/// Probe 3 — every BIF that takes a **port or a name** accepts a registered port's name.
///
/// Oracle answer:
/// ```text
/// {foo,1}
/// true
/// true
/// name
/// {registered_name,myp}
/// ```
const PROBE_REGISTERED_NAME: &str = r#"P = open_port({spawn_driver,"ram_file_drv"},[]),
true = erlang:port_set_data(P,{foo,1}),
true = erlang:register(myp,P),
io:format("~p~n",[erlang:port_get_data(myp)]),
io:format("~p~n",[erlang:port_set_data(myp,{a,2})]),
io:format("~p~n",[erlang:port_connect(myp,self())]),
io:format("~p~n",[element(1,erlang:port_info(myp,name))]),
io:format("~p~n",[hd(erlang:port_info(myp))]),
halt(0).
"#;

/// Probe 4 — the badarg matrix. `F/1` runs a thunk and renders a caught raise as its **bare reason
/// atom**, so each line is the class-independent answer `badarg` (never a stacktrace).
///
/// Oracle answer: thirteen lines, all `badarg` — `port_set_data`/`port_get_data`/`port_connect` on a
/// pid and on an integer, the same three on a **closed** port, `port_connect` to a dead pid,
/// `register/2` of an already-registered or already-named port, `register(init, P)` on the name a
/// process holds, and `unregister/1` of an unknown name.
const PROBE_BADARG_MATRIX: &str = r#"F = fun(G) -> case catch G() of {'EXIT',{R,_}} when is_atom(R) -> R; {'EXIT',R} when is_atom(R) -> R; V -> {ok,V} end end,
P = open_port({spawn_driver,"ram_file_drv"},[]),
true = erlang:register(zzp,P),
Dead = list_to_pid("<0.9999.0>"),
io:format("~p~n",[F(fun() -> erlang:port_set_data(self(),{x,1}) end)]),
io:format("~p~n",[F(fun() -> erlang:port_get_data(self()) end)]),
io:format("~p~n",[F(fun() -> erlang:port_connect(self(),self()) end)]),
io:format("~p~n",[F(fun() -> erlang:port_set_data(3,{x,1}) end)]),
io:format("~p~n",[F(fun() -> erlang:port_get_data(3) end)]),
io:format("~p~n",[F(fun() -> erlang:port_connect(3,self()) end)]),
Closed = open_port({spawn_driver,"ram_file_drv"},[]),
true = erlang:port_close(Closed),
io:format("~p~n",[F(fun() -> erlang:port_set_data(Closed,{x,1}) end)]),
io:format("~p~n",[F(fun() -> erlang:port_get_data(Closed) end)]),
io:format("~p~n",[F(fun() -> erlang:port_connect(Closed,self()) end)]),
io:format("~p~n",[F(fun() -> erlang:port_connect(P,Dead) end)]),
io:format("~p~n",[F(fun() -> erlang:register(zzq,P) end)]),
io:format("~p~n",[F(fun() -> erlang:register(init,P) end)]),
io:format("~p~n",[F(fun() -> erlang:unregister(nosuchname) end)]),
halt(0).
"#;

/// Probe 5 — an **owner transfer** to a real OS child (`{spawn, "cat"}`): the opener hands the port
/// to a helper, the helper receives the child's echo, and the opener reads the port's `links` and
/// `connected` back.
///
/// Oracle answer:
/// ```text
/// transfer={true,<<"hello">>}
/// 2
/// true
/// ```
///
/// Nothing printed is a raw pid (`port_info(P, connected) =:= Helper` is a boolean, `links` only its
/// length), and every `receive` is bounded, so neither runtime can hang. The helper outlives the
/// opener's two prints (its trailing `receive` is bounded by the runtime's own exit anyway), which
/// is what keeps `port_info/2` from racing the port's own teardown.
const PROBE_OWNER_TRANSFER: &str = r#"Parent = self(),
P = open_port({spawn,"cat"},[binary]),
Helper = spawn(fun() ->
    receive go -> ok after 2000 -> ok end,
    R = receive {P2,{data,D}} -> {is_port(P2),D} after 2000 -> timeout end,
    io:format("transfer=~p~n",[R]),
    Parent ! helper_done,
    receive stop -> ok after 500 -> ok end
end),
true = erlang:port_connect(P,Helper),
Helper ! go,
true = erlang:port_command(P,<<"hello">>),
receive helper_done -> ok after 2000 -> ok end,
io:format("~p~n",[length(element(2,erlang:port_info(P,links)))]),
io:format("~p~n",[element(2,erlang:port_info(P,connected)) =:= Helper]),
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
            "SKIP the port_bif probe: the oracle could not answer\n--- expression ---\n{expr}"
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

/// Probe 1: the data term, the owner re-connect, the name registration and `port_close/1`.
#[test]
fn the_spawn_driver_sequence_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("port_bif") else {
        return;
    };
    agrees(&oracle, PROBE_SPAWN_DRIVER);
}

/// Probe 2: a fresh port has no data term (`undefined`) and no name (`[]`).
#[test]
fn a_fresh_port_has_no_data_and_no_name() {
    let Some(oracle) = support::require_oracle("port_bif") else {
        return;
    };
    agrees(&oracle, PROBE_FRESH_PORT);
}

/// Probe 3: `port_set_data`/`port_get_data`/`port_connect`/`port_info` accept a registered name.
#[test]
fn a_registered_name_addresses_the_port() {
    let Some(oracle) = support::require_oracle("port_bif") else {
        return;
    };
    agrees(&oracle, PROBE_REGISTERED_NAME);
}

/// Probe 4: the thirteen-arm badarg matrix.
#[test]
fn the_badarg_matrix_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("port_bif") else {
        return;
    };
    agrees(&oracle, PROBE_BADARG_MATRIX);
}

/// Probe 5: an owner transfer to a real OS child delivers the echo to the new owner.
#[test]
fn an_owner_transfer_delivers_to_the_new_owner() {
    let Some(oracle) = support::require_oracle("port_bif") else {
        return;
    };
    agrees(&oracle, PROBE_OWNER_TRANSFER);
}
