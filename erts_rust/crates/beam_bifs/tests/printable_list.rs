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

//! Integration tests for `io_lib:printable_list/1` over the real registry.
//!
//! Every expectation is pinned against the system oracle
//! (`erl -noshell -eval 'io:format("~p~n",[io_lib:printable_list(T)])'`,
//! OTP 29 / erts-17.1). The BIF is a pure function, so no optional seam is needed.
//!
//! The rule (`../../lib/stdlib/src/io_lib.erl:1602-1610`,
//! `printable_latin1_list/1` via `printable_character/2` with `latin1`) is: `true`
//! iff the argument is a **proper** list every element of which is an integer in
//! `16#20..=16#7E`, in `16#A0..=16#FF`, or one of `\t \n \r \v \b \f \e`.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext};
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, AtomTable, Eterm, NIL};

const ATOMS: [&str; 4] = ["true", "false", "abc", "a"];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Calls `io_lib:printable_list(Term)` and returns the boolean result.
fn printable_list(elements: impl FnOnce(&mut BifContext<'_>) -> Result<Eterm>) -> bool {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let term = elements(&mut ctx).expect("the term fits the test heap");
    let result = reg
        .call("io_lib", "printable_list", 1, &mut ctx, &[term])
        .expect("printable_list/1 is registered");
    match ctx.atoms().name(atom_val(result)) {
        Some("true") => true,
        Some("false") => false,
        other => panic!("printable_list returned a non-boolean: {other:?}"),
    }
}

/// A proper list of small integers.
fn proper(ctx: &mut BifContext<'_>, values: &[i64]) -> Result<Eterm> {
    let elems: Vec<Eterm> = values.iter().map(|&v| make_small(v)).collect();
    ctx.list(&elems)
}

#[test]
fn the_empty_list_is_printable() {
    // oracle: io_lib:printable_list([]) => true
    assert!(printable_list(|_| Ok(NIL)));
}

#[test]
fn ascii_strings_are_printable() {
    // oracle: io_lib:printable_list("abc") => true
    assert!(printable_list(|ctx| proper(ctx, &[97, 98, 99])));
    // The latin1-printable boundaries: 16#20 and 16#7E are printable.
    assert!(printable_list(|ctx| proper(ctx, &[32])));
    assert!(printable_list(|ctx| proper(ctx, &[126])));
}

#[test]
fn low_control_bytes_are_not_printable() {
    // oracle: io_lib:printable_list([1,2,3]) => false (and [127]/[31] => false)
    assert!(!printable_list(|ctx| proper(ctx, &[1, 2, 3])));
    assert!(!printable_list(|ctx| proper(ctx, &[127])));
    assert!(!printable_list(|ctx| proper(ctx, &[31])));
    assert!(!printable_list(|ctx| proper(ctx, &[0])));
}

#[test]
fn the_named_control_characters_are_printable() {
    // oracle: io_lib:printable_list([9,10,13,11,8,12,27]) => true
    // (\t \n \r \v \b \f \e)
    assert!(printable_list(|ctx| proper(ctx, &[9, 10, 13, 11, 8, 12, 27])));
}

#[test]
fn the_latin1_high_range_is_printable() {
    // oracle: io_lib:printable_list([160]) => true, [200] => true, [255] => true,
    //         [128] => false (the 16#80..16#9F C1 control block is NOT printable).
    assert!(!printable_list(|ctx| proper(ctx, &[128])));
    assert!(printable_list(|ctx| proper(ctx, &[160])));
    assert!(printable_list(|ctx| proper(ctx, &[200])));
    assert!(printable_list(|ctx| proper(ctx, &[255])));
}

#[test]
fn out_of_latin1_bytes_are_not_printable() {
    // oracle: io_lib:printable_list([256]) => false, [-1] => false
    assert!(!printable_list(|ctx| proper(ctx, &[256])));
    assert!(!printable_list(|ctx| proper(ctx, &[-1])));
}

#[test]
fn an_improper_list_is_not_printable() {
    // oracle: io_lib:printable_list([97,98|99]) => false
    assert!(!printable_list(|ctx| {
        let tail = make_small(99);
        let inner = ctx.cons(make_small(98), tail)?;
        ctx.cons(make_small(97), inner)
    }));
}

#[test]
fn a_non_integer_element_is_not_printable() {
    // oracle: [a] => false, [1.5] => false, [{1}] => false
    assert!(!printable_list(|ctx| {
        let atom = ctx.atom("a")?;
        ctx.list(&[atom])
    }));
    assert!(!printable_list(|ctx| {
        let float = ctx.make_float(1.5)?;
        ctx.list(&[float])
    }));
    assert!(!printable_list(|ctx| {
        let tuple = ctx.tuple(&[make_small(1)])?;
        ctx.list(&[tuple])
    }));
}

#[test]
fn a_non_list_argument_is_not_printable() {
    // oracle: io_lib:printable_list(abc) => false
    assert!(!printable_list(|ctx| ctx.atom("abc")));
    assert!(!printable_list(|_| Ok(make_small(97))));
}

#[test]
fn a_trailing_non_printable_element_is_false() {
    // oracle: io_lib:printable_list("abc" ++ [1]) => false
    assert!(!printable_list(|ctx| proper(ctx, &[97, 98, 99, 1])));
}

#[test]
fn printable_list_is_registered() {
    assert!(default_registry().contains("io_lib", "printable_list", 1));
}
