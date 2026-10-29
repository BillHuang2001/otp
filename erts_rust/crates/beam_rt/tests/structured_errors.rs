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
//! * the process runner's `signalled_reason` — a process that raises a structured reason
//!   must die with ERTS's **exit value** `{Reason, Stacktrace}`, the term
//!   [`Runtime::process_exit`] records and a link's `{'EXIT', Pid, R}`/a monitor's `'DOWN'`
//!   carries. The *reason* half is the very term the raise built.
//!
//! The producers are correct (`beam_bifs` builds the real term and carries it in the
//! `beam_common::Raise` live-reason slot); these tests pin the consumers.
//!
//! Oracle (OTP 29 / `erts-17.1`, verified with `erl -noshell` and the committed
//! `raise_structured.beam`: a trapping process `spawn_link`s the function and prints the
//! `{'EXIT', Pid, R}` it gets):
//!
//! ```erl
//! spawn_link(raise_structured, badkey, []).
//! %% {{badkey,nokey},
//! %%  [{erlang,map_get,[nokey,#{}],[{error_info,#{module => erl_erts_errors}}]},
//! %%   {raise_structured,badkey,0,[{file,"raise_structured.erl"},{line,4}]}]}
//! spawn_link(raise_structured, badmap, []).
//! %% {{badmap,not_a_map},
//! %%  [{erlang,map_get,[a,not_a_map],[{error_info,#{module => erl_erts_errors}}]},
//! %%   {raise_structured,badmap,0,[{file,"raise_structured.erl"},{line,5}]}]}
//! ```
//!
//! The catch-side rows below are the other half of the same oracle session:
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

/// One decoded stacktrace item — ERTS's `{Module, Function, Arity, Location}`.
///
/// `file`/`line` are `None` for a frame whose location is not a `[{file, _}, {line, _}]`
/// list, which is the oracle's own shape for a frame with no source position (`[]`) and for
/// a BIF frame's `[{error_info, #{…}}]`.
#[derive(Debug, PartialEq)]
struct Frame {
    module: String,
    function: String,
    arity: i64,
    file: Option<String>,
    line: Option<i64>,
}

/// An Erlang **string** (a list of small integers) as Rust text.
fn string_of(term: Eterm) -> Option<String> {
    let units = terms::list_elements(term).ok()?;
    let mut text = String::new();
    for unit in units {
        if !is_small(unit) {
            return None;
        }
        text.push(char::from(u8::try_from(signed_value(unit)).ok()?));
    }
    Some(text)
}

/// Decodes a `[{file, File}, {line, Line}]` location, or `(None, None)` for any other term.
fn location_of(rt: &Runtime, term: Eterm) -> (Option<String>, Option<i64>) {
    let Ok(items) = terms::list_elements(term) else {
        return (None, None);
    };
    let (mut file, mut line) = (None, None);
    for item in items {
        if terms::tuple_arity(item).ok() != Some(2) {
            continue;
        }
        let key = terms::tuple_element(item, 0).expect("the location key");
        let value = terms::tuple_element(item, 1).expect("the location value");
        // A `{file, S}` pair carries an Erlang string, a `{line, N}` pair a small int; any
        // other pair (`{error_info, #{…}}` on a BIF frame) is not a source position.
        match rt.atom_name_of(key) {
            Some("file") => file = string_of(value),
            Some("line") if is_small(value) => line = Some(signed_value(value)),
            _ => {}
        }
    }
    (file, line)
}

/// The **exit value** a process died with, split into its reason and its stacktrace.
///
/// ERTS signals `{Reason, Stacktrace}` for an `error`-class death, so the pair — never the
/// bare reason — is what [`Runtime::process_exit`] records.
fn as_exit_value(rt: &Runtime, term: Eterm) -> (Eterm, Vec<Frame>) {
    assert_eq!(
        terms::tuple_arity(term).expect("an exit value is a tuple"),
        2,
        "an `error`-class exit value is `{{Reason, Stacktrace}}`"
    );
    let reason = terms::tuple_element(term, 0).expect("the reason slot");
    let trace = terms::tuple_element(term, 1).expect("the stacktrace slot");
    let items = terms::list_elements(trace).expect("the exit value carries a stacktrace list");
    let frames = items
        .into_iter()
        .map(|item| {
            assert_eq!(
                terms::tuple_arity(item).expect("each stacktrace item is a tuple"),
                4,
                "a stacktrace item is `{{Module, Function, Arity, Location}}`"
            );
            let module = terms::tuple_element(item, 0).expect("the module slot");
            let function = terms::tuple_element(item, 1).expect("the function slot");
            let arity = terms::tuple_element(item, 2).expect("the arity slot");
            let location = terms::tuple_element(item, 3).expect("the location slot");
            let (file, line) = location_of(rt, location);
            Frame {
                module: rt
                    .atom_name_of(module)
                    .expect("a frame's module is a nameable atom")
                    .to_string(),
                function: rt
                    .atom_name_of(function)
                    .expect("a frame's function is a nameable atom")
                    .to_string(),
                arity: signed_value(arity),
                file,
                line,
            }
        })
        .collect();
    (reason, frames)
}

/// The frame naming `module:function/arity`, with its source position.
fn dying_frame<'a>(frames: &'a [Frame], module: &str, function: &str) -> &'a Frame {
    frames
        .iter()
        .find(|frame| frame.module == module && frame.function == function)
        .unwrap_or_else(|| panic!("no `{module}:{function}` frame in {frames:?}"))
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

    let exit_value = rt
        .process_exit(pid)
        .expect("the raising process records the exit value it died with");
    // The exit value's **reason** is the real `{badkey, nokey}` term — NOT an interned
    // diagnostic, and not wrapped: the pair's first element is that very tuple.
    let (reason, frames) = as_exit_value(&rt, exit_value);
    assert_eq!(as_atom_pair(&rt, reason), ("badkey".to_string(), "nokey".to_string()));
    // Its **stacktrace** names the dying function with the source position the oracle
    // reports (`{raise_structured,badkey,0,[{file,"raise_structured.erl"},{line,4}]}`).
    let dying = dying_frame(&frames, "raise_structured", "badkey");
    assert_eq!(dying.arity, 0);
    assert_eq!(dying.file.as_deref(), Some("raise_structured.erl"));
    assert_eq!(dying.line, Some(4));
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

    let exit_value = rt
        .process_exit(pid)
        .expect("the raising process records the exit value it died with");
    let (reason, frames) = as_exit_value(&rt, exit_value);
    assert_eq!(
        as_atom_pair(&rt, reason),
        ("badmap".to_string(), "not_a_map".to_string())
    );
    let dying = dying_frame(&frames, "raise_structured", "badmap");
    assert_eq!(dying.arity, 0);
    assert_eq!(dying.file.as_deref(), Some("raise_structured.erl"));
    assert_eq!(dying.line, Some(5));
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
