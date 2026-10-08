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

//! Vertical-slice integration test: load a real `.beam` and run it end to end.
//!
//! This is the engine's key Milestone-1 evidence — it drives
//! `beam_loader::load_file` over the committed oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`, reused by relative path so there is
//! a single source of truth for the image) and executes it on the interpreter
//! with a [`MinimalContext`] whose BIF hook implements `erlang:'+'/2` and
//! `erlang:'-'/2` over small integers.

use beam_common::BeamError;
use beam_engine::{Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        _ => Err(BeamError::BadArg),
    }
}

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

#[test]
fn module_and_exports_are_as_expected() {
    let module = slice_module();
    assert_eq!(module.atom_name(module.name), Some("slice"));
    for (name, arity) in [("add", 2), ("fib", 1), ("pick", 1)] {
        assert!(
            module.export(atom(&module, name), arity).is_some(),
            "{name}/{arity} must be exported"
        );
    }
}

#[test]
fn add_returns_the_sum() {
    let module = slice_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let result = machine
        .run_export(
            &module,
            atom(&module, "add"),
            2,
            &[make_small(2), make_small(3)],
            &mut ctx,
        )
        .expect("add(2, 3)");
    assert_eq!(result, make_small(5));
}

#[test]
fn fib_returns_the_tenth_fibonacci_number() {
    let module = slice_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let result = machine
        .run_export(
            &module,
            atom(&module, "fib"),
            1,
            &[make_small(10)],
            &mut ctx,
        )
        .expect("fib(10)");
    assert_eq!(result, make_small(55));
}

#[test]
fn pick_matches_the_a_clause() {
    let module = slice_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let result = machine
        .run_export(
            &module,
            atom(&module, "pick"),
            1,
            &[make_atom(atom(&module, "a"))],
            &mut ctx,
        )
        .expect("pick(a)");
    assert_eq!(result, make_small(1));
}

#[test]
fn pick_falls_through_to_the_catch_all_clause() {
    let module = slice_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let result = machine
        .run_export(
            &module,
            atom(&module, "pick"),
            1,
            &[make_atom(atom(&module, "slice"))],
            &mut ctx,
        )
        .expect("pick(slice)");
    assert_eq!(result, make_small(2));
}
