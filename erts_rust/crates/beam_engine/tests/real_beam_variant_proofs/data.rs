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

//! **Data-access variants on real compiler output**: `GetTupleElement`, `GetList`,
//! `GetHd`, `GetTl`, `UpdateRecord`, `Badrecord`.
//!
//! Sources: the committed `erlc` fixtures under `tests/fixtures/` (and, where a
//! variant is best reached through library code, a module of the installed system
//! OTP). Every row asserts the dynamic census count of its variant **and** the
//! oracle's answer for the same call — see the crate docs of
//! `real_beam_variant_proofs.rs` for the proof shape and `harness` for the
//! implementation.

use beam_term::make_small;

use crate::harness::{host, oracle_call, render, run, skip_if_unavailable};
use crate::support::{term_atom, term_tuple};

/// `GetTupleElement` — `get_tuple_element` in a `case` head, from committed
/// compiler output.
///
/// `tuples.beam:first/1` is `case T of {A,_B} -> A end`, so the element read is
/// the compiler's `get_tuple_element {x,0},1` (see
/// `tests/fixtures/README.md`), not a hand-built instruction.
///
/// Oracle: `erl -noshell -pa tests/fixtures -eval 'io:format("~w~n",[tuples:first({a,1})]),halt(0).'`
/// → `a`.
#[test]
fn get_tuple_element_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("tuples", "tuples.beam")]);
    let tag = term_atom(&mut host, "a");
    let tuple = term_tuple(&mut host, &[tag, make_small(1)]);
    let proof = run(&mut host, "tuples", "first", &[tuple]);
    proof.require(&["GetTupleElement"]);
    assert_eq!(render(&host, proof.value()), oracle_call("tuples", "first", "{a,1}"));
}
