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

//! `open_port`/`port_info`/`port_command` **parity with the system oracle** (contract A.4),
//! pinned at the [`Runtime`] library boundary.
//!
//! Every expected value below was reproduced against the system Erlang (OTP 29.1.1 /
//! erts-17.1); the oracle expression and its result are quoted at each test. The assertions
//! deliberately inspect the *term* the evaluator produced (its atom names, its shape) rather
//! than only a printed line, so a divergence cannot hide behind the `~p` printer.
//!
//! The cases are the port-surface divergences this crate closed:
//!
//! * a missing `{spawn_executable, Path}` raises `error:enoent` (`beam_io`'s errno-derived
//!   spawn-failure raise, not a generic `BeamError::Io`);
//! * `port_command(Port, 42)` is `error:badarg` — a bare integer is **not** iodata, while
//!   `[42]` (the same integer as a *list element*) is `true`;
//! * `port_info(Port, registered_name)` of a live, **unregistered** port is the bare empty
//!   list `[]` (never a `{registered_name, _}` pair);
//! * as before, an empty `{spawn, ""}`/`{spawn_executable, ""}` name is `error:badarg` and
//!   consumes no port number, and `port_info` of a closed/unknown port is `undefined`.
use beam_engine::terms;
use beam_rt::{InfoValue, Runtime};
use beam_term::{is_atom, is_nil, Eterm};

/// A fresh runtime (boot on demand).
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// The `{Class, Reason}` a self-terminating `try … catch Class:Reason -> {Class, Reason} end`
/// produced, as `(class, reason)` atom names.
fn caught(rt: &mut Runtime, expression: &str) -> (Option<String>, Option<String>) {
    let value = rt.eval(expression).expect("the eval runs");
    assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2, "{value:?}");
    let class = terms::tuple_element(value, 0).expect("the class");
    let reason = terms::tuple_element(value, 1).expect("the reason");
    assert!(is_atom(class) && is_atom(reason), "atom class/reason, got {value:?}");
    (
        rt.atom_name_of(class).map(str::to_string),
        rt.atom_name_of(reason).map(str::to_string),
    )
}

/// The atom `rt.eval(expression)` returned, by name.
fn atom_of(rt: &mut Runtime, expression: &str) -> String {
    let value = rt.eval(expression).expect("the eval runs");
    assert!(is_atom(value), "expected an atom, got {value:?}");
    rt.atom_name_of(value).expect("a named atom").to_string()
}

/// The live port count the runtime reports (`system_info(port_count)`).
fn port_count(rt: &Runtime) -> i64 {
    match rt.system_info_value("port_count").expect("port_count is a key") {
        InfoValue::Int(n) => n,
        other => panic!("port_count must be an integer, got {other:?}"),
    }
}

/// `open_port({spawn_executable, "/no/such/path/xyz"}, [])` ⇒ `error:enoent`.
///
/// The oracle answers `erl_errno_id(err_num)` for a failed child (`erl_bif_port.c:84`);
/// `beam_io`'s `SpawnDriver` preserves `std::io::Error`'s `raw_os_error()` and maps `ENOENT`
/// to the `enoent` atom raise, so the runtime surfaces the oracle's own reason rather than a
/// generic `BeamError::Io` (which would exit 1 with a different message).
#[test]
fn a_missing_spawn_executable_raises_enoent() {
    let mut rt = runtime();
    let (class, reason) = caught(
        &mut rt,
        "try open_port({spawn_executable, \"/no/such/path/xyz\"}, []) \
         catch C:E -> {C,E} end",
    );
    assert_eq!(class.as_deref(), Some("error"));
    assert_eq!(reason.as_deref(), Some("enoent"));
}

/// `port_command(Port, 42)` ⇒ `error:badarg`, while `port_command(Port, [42])` ⇒ `true`.
///
/// `iodata() = binary() | iolist()`: an integer is a valid **element** (`42` is the byte
/// `"*"`) but never a top-level iodata term (`iolist_to_binary(42)` is `badarg`).
#[test]
fn port_command_rejects_a_bare_integer_but_accepts_it_as_an_element() {
    let mut rt = runtime();
    let (class, reason) = caught(
        &mut rt,
        "begin P = open_port({spawn,\"cat\"}, []), \
         try erlang:port_command(P, 42) catch C:E -> {C,E} end end",
    );
    assert_eq!(class.as_deref(), Some("error"));
    assert_eq!(reason.as_deref(), Some("badarg"));

    // The same integer as a list **element** is iodata, and the write answers `true`.
    let value = rt
        .eval("begin P = open_port({spawn,\"cat\"}, []), erlang:port_command(P, [42]) end")
        .expect("`[42]` is iodata");
    assert_eq!(rt.atom_name_of(value), Some("true"));
}

/// `port_info(Port, registered_name)` of a live, unregistered port ⇒ the bare `[]`.
///
/// `beam_rt` registers no ports (`erlang:register/2` registers pids only), so a live port is
/// always unregistered and `erlang.erl` passes `erts_internal:port_info/2`'s `[]` straight
/// through — never a `{registered_name, _}` pair. The `/1` roster never includes the item.
#[test]
fn port_info_registered_name_of_a_live_port_is_the_empty_list() {
    let mut rt = runtime();
    let value = rt
        .eval(
            "begin P = open_port({spawn,\"cat\"}, []), \
             erlang:port_info(P, registered_name) end",
        )
        .expect("the live port's registered_name is claimed");
    assert!(is_nil(value), "the bare empty list, got {value:?}");

    // The other items still come from the `beam_bifs` pair list (`name` is one of them).
    let value = rt
        .eval("begin P = open_port({spawn,\"cat\"}, []), erlang:port_info(P, name) end")
        .expect("a live port's name item");
    assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2, "{value:?}");
    let name = terms::tuple_element(value, 1).expect("the name value");
    assert!(terms::binary_bytes(name).is_err(), "the default stream is a char list");
    let bytes: Vec<i64> = terms::list_elements(name)
        .expect("a char list")
        .into_iter()
        .map(|element| beam_term::signed_value(element))
        .collect();
    assert_eq!(bytes, b"cat".iter().map(|b| i64::from(*b)).collect::<Vec<_>>());
}

/// An empty `{spawn, ""}`/`{spawn_executable, ""}` name ⇒ `error:badarg`, and **no** port
/// number is consumed (the oracle rejects such a name before the port is created).
#[test]
fn an_empty_spawn_name_is_badarg_and_consumes_no_port() {
    let mut rt = runtime();
    for name in ["{spawn, \"\"}", "{spawn_executable, \"\"}"] {
        let before = port_count(&rt);
        let (class, reason) = caught(
            &mut rt,
            &format!("try open_port({name}, []) catch C:E -> {{C,E}} end"),
        );
        assert_eq!(class.as_deref(), Some("error"), "{name}");
        assert_eq!(reason.as_deref(), Some("badarg"), "{name}");
        assert_eq!(port_count(&rt), before, "{name} must not consume a port");
    }
}

/// `port_info` of a closed/unknown port ⇒ the atom `undefined`, for **both** arities and any
/// item (the `undefined` answer precedes the item lookup).
///
/// Oracle: `port_info(ClosedPort)` / `port_info(ClosedPort, registered_name)` /
/// `port_info(<unknown>, registered_name)` all ⇒ `undefined`.
#[test]
fn port_info_of_a_closed_or_unknown_port_is_undefined() {
    let mut rt = runtime();
    // A closed port.
    let value = rt
        .eval(
            "begin P = open_port({spawn,\"cat\"}, []), port_close(P), \
             {erlang:port_info(P), erlang:port_info(P, registered_name)} end",
        )
        .expect("the closed-port items");
    for index in [0, 1] {
        let item = terms::tuple_element(value, index).expect("an item");
        assert!(is_atom(item), "item {index} must be an atom, got {value:?}");
        assert_eq!(rt.atom_name_of(item), Some("undefined"), "item {index}");
    }
    // A port the runtime has never held.
    assert_eq!(
        atom_of(
            &mut rt,
            "erlang:port_info(list_to_port(\"#Port<0.999>\"), registered_name)",
        ),
        "undefined"
    );
}

/// The empty list is iodata: `port_command(Port, [])` ⇒ `true` (the oracle's own value), so
/// the bare-integer rejection above does not over-reject a legitimate empty write.
#[test]
fn port_command_accepts_an_empty_iolist() {
    let mut rt = runtime();
    let value = rt
        .eval("begin P = open_port({spawn,\"cat\"}, []), erlang:port_command(P, []) end")
        .expect("`[]` is iodata");
    assert_eq!(rt.atom_name_of(value), Some("true"));

    let value: Eterm = rt
        .eval("begin P = open_port({spawn,\"cat\"}, []), erlang:port_command(P, <<>>) end")
        .expect("`<<>>` is iodata");
    assert_eq!(rt.atom_name_of(value), Some("true"));
}
