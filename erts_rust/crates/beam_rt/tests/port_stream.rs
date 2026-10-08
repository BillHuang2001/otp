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

//! The port-stream read at the **library** boundary (the `RuntimeContext` path).
//!
//! `tests/eval.rs` pins the corpus golden at the CLI boundary (`driver/spawn_driver_port_stream_read`
//! → `ok <<104,105,10>>`). Here the **same** path is exercised through [`Runtime::eval`] so
//! the *term* the receive delivers can be asserted structurally — `{Port, {data, Data}}`
//! with `Data` a `binary` (or a char list on the default `list` stream) — rather than only
//! through the `~w` printer.
//!
//! The receive runs the evaluator's `after` branch against
//! [`RuntimeContext::poll_port_messages`](beam_rt::RuntimeContext), which polls the child's
//! stdout through a `beam_io::Poller` and builds the message on the runtime's arena. The
//! terms therefore stay valid after `eval` returns (the runtime's arena is never reset).
use beam_engine::terms;
use beam_rt::Runtime;
use beam_process::PortId;
use beam_term::{is_atom, is_small, signed_value, Eterm};

/// A fresh runtime (boot on demand).
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// `term` as `{Port, {data, Data}}` — the one shape a port stream read produces — returning
/// `(Port, Data)`.
fn as_data_message(term: Eterm) -> (Eterm, Eterm) {
    assert_eq!(terms::tuple_arity(term).expect("a 2-tuple"), 2, "{term:?}");
    let port = terms::tuple_element(term, 0).expect("the port");
    let inner = terms::tuple_element(term, 1).expect("the {data, Data} pair");
    assert_eq!(terms::tuple_arity(inner).expect("a 2-tuple"), 2);
    let tag = terms::tuple_element(inner, 0).expect("the tag");
    assert!(is_atom(tag), "the tag must be an atom");
    let data = terms::tuple_element(inner, 1).expect("the payload");
    (port, data)
}

#[test]
fn a_binary_port_stream_read_yields_the_data_message() {
    let mut rt = runtime();
    // A single-variable clause (`receive M -> M`) accepts whatever the poller delivers, so
    // the whole message can be inspected.
    let message = rt
        .eval(
            "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
             receive M -> M after 500 -> no_data end end",
        )
        .expect("the receive delivers a port message");

    let (port, data) = as_data_message(message);
    // The port term is the very id `open_port` handed out (the message's `P`).
    assert!(
        PortId::from_eterm(port).is_ok(),
        "the message's first element must be an internal port"
    );
    // The tag is `data`, resolved through the runtime's shared atom table.
    assert_eq!(
        rt.atom_name_of(terms::tuple_element(terms::tuple_element(message, 1).unwrap(), 0).unwrap()),
        Some("data")
    );
    // `echo hi` writes exactly `"hi\n"` on a `binary` stream.
    assert_eq!(terms::binary_bytes(data).expect("a binary"), b"hi\n");
}

#[test]
fn the_bound_port_variable_matches_the_message() {
    let mut rt = runtime();
    // The clause's `P` is already bound: the match only succeeds because the message's port
    // term is exactly the `open_port` result (`element(1, M) =:= P`).
    let value = rt
        .eval(
            "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
             receive M -> element(1, M) =:= P after 500 -> no_data end end",
        )
        .expect("the clause matches the bound port");
    assert_eq!(rt.atom_name_of(value), Some("true"));
}

#[test]
fn a_list_port_stream_read_yields_a_charlist() {
    let mut rt = runtime();
    // The `list` default (no `binary` option) delivers a char list, not a binary.
    let message = rt
        .eval(
            "begin P = open_port({spawn,\"echo hi\"}, []), \
             receive M -> M after 500 -> no_data end end",
        )
        .expect("the receive delivers a port message");
    let (_port, data) = as_data_message(message);
    assert!(terms::binary_bytes(data).is_err(), "a list stream is not a binary");
    let bytes: Vec<i64> = terms::list_elements(data)
        .expect("a char list")
        .into_iter()
        .map(|element| {
            assert!(is_small(element), "a char list element must be a small int");
            signed_value(element)
        })
        .collect();
    assert_eq!(bytes, vec![104, 105, 10]);
}

#[test]
fn an_empty_port_stream_falls_back_to_the_after_body() {
    let mut rt = runtime();
    // `true` writes nothing: the `after 200` fires well within the poll budget, so the
    // receive must not hang and must not fabricate a `{data, …}` message.
    let value = rt
        .eval(
            "begin P = open_port({spawn,\"true\"}, [binary]), \
             receive {P,{data,D}} -> D after 200 -> no_data end end",
        )
        .expect("the after body runs");
    assert!(is_atom(value), "the fallback is an atom, not a message");
    assert_eq!(rt.atom_name_of(value), Some("no_data"));
}
