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
//! Contract A.6/A.8 — **the action's result outlives the action**, and the action leaves
//! **no process** behind.
//!
//! `Runtime::eval`/`Runtime::run_named` run their body in a freshly spawned, unregistered
//! action process (`Runtime::as_action_process`, `src/runtime.rs`) which is **ended when the
//! action is over** — the oracle's action process exits `normal` at the same point. Two
//! invariants follow, and this suite pins both:
//!
//! 1. **The result is still valid afterwards.** The value the action built lives on the
//!    *action's* heap, which dies with it, so the runtime copies it onto its pointer-stable
//!    store before ending the process — exactly what the runner does for the `x(0)` of a
//!    scheduled process (`crate::runner`: "copy it onto the runtime's pointer-stable arena
//!    (so it outlives the process, whose own heap dies with it)"). The structural read-back
//!    below is a **use-after-free** when that copy is missing: the term's `Eterm` keeps
//!    pointing at a released heap, and reading it back yields a tuple header, list element or
//!    atom read out of reused memory (observed live as `a 2-tuple: BadArg` from
//!    `tests/port_stream.rs` and as `{error,"rt"}`/`a printable term: BadArg` from
//!    `tests/file_server.rs`).
//!
//!    The churn between the build and the read is deliberate: it hands the allocator the
//!    freed block back, so a missing copy is *loud* rather than lucky.
//! 2. **No process is left behind.** `erlang:processes/0`/`system_info(process_count)` must
//!    describe the node the action left, not the action's own scaffolding. (The action's
//!    process *is* live **inside** the action, and counted there — the oracle's `-eval`
//!    process is too.)
//!
//! The rendering of the value is cross-checked against the system Erlang oracle where it is
//! oracle-visible; a missing toolchain is a loud skip, never a silent pass.
#[allow(dead_code)]
mod support;

use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{make_small, signed_value, Eterm};

/// A fresh runtime (boot on demand).
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Whether the **system stdlib** is reachable from the code path (`lists.beam`), or a loud
/// skip. Probed on a bare [`beam_rt::CodeServer`] (the `tests/function_clause.rs` shape), so
/// the check never depends on the runtime the test is about.
fn system_stdlib_available() -> bool {
    let mut server = beam_rt::CodeServer::new();
    match server.add_system_paths() {
        Ok(()) if server.find_file("lists").is_some() => true,
        Ok(()) => {
            support::skip_loudly(
                "action_result",
                "the system code path holds no lists.beam, so `run_named` cannot reach it",
            );
            false
        }
        Err(error) => {
            support::skip_loudly("action_result", &format!("no system stdlib ({error})"));
            false
        }
    }
}

/// Reads `term` back as `{tag, [1,2,3,4,5], "hello"}` — every read the action's death would
/// have broken: the tuple's arity (`tuple_arity`), its elements (`tuple_element`), the atom
/// (`atom_name_of`), the list (`list_elements`) and the char list (`list_elements`).
fn assert_the_container_is_intact(rt: &Runtime, term: Eterm) {
    assert_eq!(
        terms::tuple_arity(term).expect("a 3-tuple header"),
        3,
        "the result's tuple header must still read as a 3-tuple: {term:?}"
    );
    let tag = terms::tuple_element(term, 0).expect("the tag element");
    assert_eq!(rt.atom_name_of(tag), Some("tag"), "{term:?}");
    let list = terms::tuple_element(term, 1).expect("the list element");
    let numbers: Vec<i64> = terms::list_elements(list)
        .expect("the list element reads as a list")
        .into_iter()
        .map(signed_value)
        .collect();
    assert_eq!(numbers, vec![1, 2, 3, 4, 5], "{term:?}");
    let text = terms::tuple_element(term, 2).expect("the char-list element");
    let chars: Vec<i64> = terms::list_elements(text)
        .expect("the char-list element reads as a list")
        .into_iter()
        .map(signed_value)
        .collect();
    assert_eq!(chars, vec![104, 101, 108, 108, 111], "{term:?}");
}

/// The `{tag, [1..5], "hello"}` container the two lifetime tests build and read back, in the
/// `-eval` grammar.
const CONTAINER_EXPR: &str = "begin M = {tag, lists:seq(1,5), \"hello\"}, M end";

/// Hands the freed action heap back to the allocator, so a result that was *not* copied out of
/// it is read out of reused memory instead of merely stale memory. Each expression allocates
/// (a 200-element list, a 256-byte binary, a 40-key map), which is enough to recycle the
/// blocks a small result leaves behind.
fn churn(rt: &mut Runtime) {
    for _ in 0..8 {
        rt.eval("lists:seq(1,200)").expect("the churn allocates");
        rt.eval("list_to_binary(lists:duplicate(256, $x))")
            .expect("the churn allocates");
        rt.eval("maps:from_list([{I, I} || I <- lists:seq(1,40)])")
            .expect("the churn allocates");
    }
}

/// An `-eval` result is read back **after the action process is gone**: its structure (tuple
/// header, atom, lists) must be intact and its `~p` rendering must be the oracle's.
#[test]
fn an_eval_result_survives_the_action_that_built_it() {
    let mut rt = runtime();
    let result = rt.eval(CONTAINER_EXPR).expect("the container builds");
    churn(&mut rt);
    assert_the_container_is_intact(&rt, result);
    let rendered = rt
        .format_term(result, None)
        .expect("a printable term")
        .trim_end()
        .to_string();
    assert_eq!(
        rendered, "{tag,[1,2,3,4,5],\"hello\"}",
        "the copied-out result must print as the value the action built"
    );
    let Some(oracle) = support::require_oracle("action_result") else {
        return;
    };
    let probe = format!("io:format(\"~p\", [{CONTAINER_EXPR}])");
    let Some(expected) = support::oracle_eval(&oracle, None, &probe) else {
        eprintln!("warning: the oracle could not answer `{CONTAINER_EXPR}`");
        return;
    };
    assert_eq!(
        rendered, expected,
        "the result's rendering must agree with the oracle"
    );
    eprintln!("the action result's rendering agrees with the oracle: {rendered}");
}

/// The `-s`/`-run` route's `Runtime::run_named` result is on the action's heap too, so it is
/// pinned the same way: `lists:seq/2` builds a fresh list *inside* the action process.
#[test]
fn a_run_named_result_survives_the_action_that_built_it() {
    if !system_stdlib_available() {
        return;
    }
    let mut rt = runtime();
    let list = rt
        .run_named("lists", "seq", 2, &[make_small(1), make_small(5)])
        .expect("lists:seq(1, 5)");
    churn(&mut rt);
    let numbers: Vec<i64> = terms::list_elements(list)
        .expect("the result reads as a list")
        .into_iter()
        .map(signed_value)
        .collect();
    assert_eq!(numbers, vec![1, 2, 3, 4, 5], "{list:?}");
    assert_eq!(
        rt.format_term(list, None)
            .expect("a printable term")
            .trim_end(),
        "[1,2,3,4,5]",
        "the copied-out result must print as the value the action built"
    );
}

/// The action process is **live inside the action** (`erlang:processes/0` and
/// `system_info(process_count)` count it — the oracle's `-eval` process is live there too) and
/// **gone after it**: the node the action leaves is the node it found.
#[test]
fn an_action_leaves_no_process_behind() {
    let mut rt = runtime();
    // The count the action itself observes, including its own process.
    let inside = rt
        .eval("erlang:system_info(process_count)")
        .expect("the count is a BIF");
    let inside = match signed_value(inside) {
        n if n > 0 => n,
        n => panic!("system_info(process_count) answered {n}"),
    };
    // Outside the action the table holds exactly one fewer process — and a second action
    // observes the same count as the first, so nothing accumulates.
    assert_eq!(
        rt.processes().len() as i64,
        inside - 1,
        "the action process must not outlive the action"
    );
    let inside_again = rt
        .eval("erlang:system_info(process_count)")
        .expect("the count is a BIF");
    assert_eq!(
        signed_value(inside_again),
        inside,
        "a second action must observe the same node as the first"
    );
    assert_eq!(
        rt.processes().len() as i64,
        inside - 1,
        "the second action must not leave a process behind either"
    );
    let registered = rt.eval("whereis(init)").expect("the root is registered");
    assert!(
        !beam_term::is_nil(registered),
        "the root process must still be registered as `init` after the actions"
    );
}
