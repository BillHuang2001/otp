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

//! Structured/uncaught raise **reasons** end to end (D6) — asserted structurally, never
//! only through the `~w` printer.
//!
//! Two consumers can lose a structured reason:
//!
//! * the `-eval` evaluator's [`flow_from_error`](beam_rt::Runtime::eval) — a `try … catch`
//!   must receive the *real* reason term (`{badkey,K}` / `{badmap,X}`) instead of the raise
//!   being reshaped into an uncatchable fatal; and
//! * the process runner's `exit_reason` — a process that raises a structured reason must
//!   expose that very term through [`Runtime::process_exit`].
//!
//! The producers are correct (`beam_bifs` builds the real term and carries it in the
//! `beam_common::Raise` live-reason slot); these tests pin the consumers.
//!
//! Oracle (OTP 29 / `erts-17.1`, verified with `erl -noshell`):
//!
//! ```erl
//! try maps:get(nokey, #{})    catch C:R -> {C,R} end.  %% {error,{badkey,nokey}}
//! try map_get(a, not_a_map)   catch C:R -> {C,R} end.  %% {error,{badmap,not_a_map}}
//! try [Y || Y <- not_a_list]                  catch C:R -> {C,R} end.
//!                                                      %% {error,{bad_generator,not_a_list}}
//! try <<Y || <<Y>> <= not_a_binary>>          catch C:R -> {C,R} end.
//!                                                      %% {error,{bad_generator,not_a_binary}}
//! maps:keys(#{3=>a,1=>b,2=>c}).                        %% [1,2,3]
//! ```
use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{is_small, signed_value, Eterm};

/// A fresh runtime (boot on demand).
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// A runtime with `fixture` loaded explicitly, so the module resolves from the code
/// server's cache exactly as a `-pa` directory would resolve it.
fn runtime_with(fixture: &str) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(fixture).expect("the committed fixture loads");
    rt
}

/// Asserts `term` is the 2-tuple `{A, B}` and returns `(name_of(A), B)`.
fn as_pair(rt: &Runtime, term: Eterm) -> (&str, Eterm) {
    assert_eq!(
        terms::tuple_arity(term).expect("a tuple"),
        2,
        "{term:?} is not a 2-tuple"
    );
    let first = terms::tuple_element(term, 0).expect("the first element");
    let name = rt.atom_name_of(first).expect("the first element is a nameable atom");
    let second = terms::tuple_element(term, 1).expect("the second element");
    (name, second)
}

/// Asserts `term` is the 2-tuple `{Atom, Atom}` and returns `(tag, value)` as strings.
fn as_atom_pair(rt: &Runtime, term: Eterm) -> (String, String) {
    let (tag, value) = as_pair(rt, term);
    let value = rt
        .atom_name_of(value)
        .expect("the second element is a nameable atom")
        .to_string();
    (tag.to_string(), value)
}

// -- Part 1a: catchable structured reasons through `Runtime::eval` -------------------

#[test]
fn a_badkey_reason_is_catchable_and_is_the_real_term() {
    let mut rt = runtime();
    // `try maps:get(nokey,#{}) catch C:R -> {C,R} end` ⇒ `{error,{badkey,nokey}}`.
    let caught = rt
        .eval("try maps:get(nokey,#{}) catch C:R -> {C,R} end")
        .expect("the structured raise is catchable");

    let (class, reason) = as_pair(&rt, caught);
    assert_eq!(class, "error");
    // The reason is the structured 2-tuple, not a bare `badkey` atom.
    assert_eq!(as_atom_pair(&rt, reason), ("badkey".to_string(), "nokey".to_string()));
}

#[test]
fn a_badmap_reason_is_catchable_and_is_the_real_term() {
    let mut rt = runtime();
    // `try map_get(a, not_a_map) catch C:R -> {C,R} end` ⇒ `{error,{badmap,not_a_map}}`.
    let caught = rt
        .eval("try map_get(a, not_a_map) catch C:R -> {C,R} end")
        .expect("the structured raise is catchable");

    let (class, reason) = as_pair(&rt, caught);
    assert_eq!(class, "error");
    assert_eq!(as_atom_pair(&rt, reason), ("badmap".to_string(), "not_a_map".to_string()));
}

#[test]
fn a_caught_structured_reason_binds_both_the_class_and_the_term() {
    let mut rt = runtime();
    // A `catch` **clause** (not the `catch Expr` shorthand) matches the tuple pattern
    // `{badkey, K}` and binds `K`, proving the reason really is that tuple.
    let key = rt
        .eval("try maps:get(nokey,#{}) catch error:{badkey,K} -> K end")
        .expect("the clause matches the structured reason");
    assert_eq!(rt.atom_name_of(key), Some("nokey"));
}

// -- Part 1b: a spawned process exposes the real term through `process_exit` ---------

/// This crate's structured-raise fixture (`tests/fixtures/raise_structured.beam`).
const RAISE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/raise_structured.beam"
);

#[test]
fn a_spawned_process_raising_badkey_exposes_the_real_term() {
    let mut rt = runtime_with(RAISE_BEAM);
    let pid = rt
        .spawn_process("raise_structured", "badkey", vec![])
        .expect("spawn raise_structured:badkey/0");
    rt.run_scheduler().expect("the raise is absorbed by the scheduler pass");

    let reason = rt
        .process_exit(pid)
        .expect("the raising process records its exit reason");
    // The exit reason is the real `{badkey, nokey}` term — NOT an interned diagnostic.
    assert_eq!(as_atom_pair(&rt, reason), ("badkey".to_string(), "nokey".to_string()));
    // A process that raised records no normal result.
    assert!(rt.process_result(pid).is_none());
}

#[test]
fn a_spawned_process_raising_badmap_exposes_the_real_term() {
    let mut rt = runtime_with(RAISE_BEAM);
    let pid = rt
        .spawn_process("raise_structured", "badmap", vec![])
        .expect("spawn raise_structured:badmap/0");
    rt.run_scheduler().expect("the raise is absorbed by the scheduler pass");

    let reason = rt
        .process_exit(pid)
        .expect("the raising process records its exit reason");
    assert_eq!(
        as_atom_pair(&rt, reason),
        ("badmap".to_string(), "not_a_map".to_string())
    );
    assert!(rt.process_result(pid).is_none());
}

// -- Part 2: `{bad_generator, X}` producers ------------------------------------------

#[test]
fn a_list_generator_over_a_non_list_raises_bad_generator() {
    let mut rt = runtime();
    // `try [Y || Y <- not_a_list] catch C:R -> {C,R} end`
    //   ⇒ `{error,{bad_generator,not_a_list}}`.
    let caught = rt
        .eval("try [Y || Y <- not_a_list] catch C:R -> {C,R} end")
        .expect("the reason is catchable");

    let (class, reason) = as_pair(&rt, caught);
    assert_eq!(class, "error");
    assert_eq!(
        as_atom_pair(&rt, reason),
        ("bad_generator".to_string(), "not_a_list".to_string())
    );
}

#[test]
fn a_list_generator_over_a_non_list_keeps_the_offending_value() {
    let mut rt = runtime();
    // The reason's second element is the *offending value* as-is: here an integer.
    let caught = rt
        .eval("try [Y || Y <- 17] catch C:R -> {C,R} end")
        .expect("the reason is catchable");
    let (_class, reason) = as_pair(&rt, caught);
    let (tag, value) = as_pair(&rt, reason);
    assert_eq!(tag, "bad_generator");
    assert!(is_small(value), "the offending value must be the integer 17 as-is");
    assert_eq!(signed_value(value), 17);
}

#[test]
fn a_binary_generator_over_a_non_binary_raises_bad_generator() {
    let mut rt = runtime();
    // `try <<Y || <<Y>> <= not_a_binary>> catch C:R -> {C,R} end`
    //   ⇒ `{error,{bad_generator,not_a_binary}}`.
    let caught = rt
        .eval("try <<Y || <<Y>> <= not_a_binary>> catch C:R -> {C,R} end")
        .expect("the reason is catchable");

    let (class, reason) = as_pair(&rt, caught);
    assert_eq!(class, "error");
    assert_eq!(
        as_atom_pair(&rt, reason),
        ("bad_generator".to_string(), "not_a_binary".to_string())
    );
}

// -- Part 3: the map literal uses canonical key order ---------------------------------

#[test]
fn a_map_literal_has_canonical_key_order() {
    let mut rt = runtime();
    // `maps:keys(#{3=>a,1=>b,2=>c})` ⇒ `[1,2,3]` (the oracle sorts the keys).
    let keys = rt
        .eval("maps:keys(#{3=>a,1=>b,2=>c})")
        .expect("the literal builds");
    let keys: Vec<i64> = terms::list_elements(keys)
        .expect("a key list")
        .into_iter()
        .map(|key| {
            assert!(is_small(key), "each key must be a small int");
            signed_value(key)
        })
        .collect();
    assert_eq!(keys, vec![1, 2, 3]);
}
