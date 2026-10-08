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

//! The end-to-end **library** vertical slice: load a real `.beam` and run it.
//!
//! This is `beam_rt`'s key Milestone-1 evidence. The engine already proves the
//! interpreter with a test-only context (`beam_engine/tests/slice.rs`); this test
//! reproduces the *same* run through the real runtime glue — `Runtime`,
//! `RuntimeContext`, the `beam_bifs` registry, the process heap and the process table —
//! and through the registered `io:format` printer.
//!
//! The image is the committed oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`), reused by relative path so there is a
//! single source of truth for it — never copied.
use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_loader::load_file;
use beam_rt::Runtime;
use beam_term::{is_list, is_small, make_atom, make_small, signed_value, Eterm};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

#[test]
fn run_function_loads_runs_and_caches_a_real_beam() {
    let mut rt = runtime();
    assert!(!rt.is_booted());

    let sum = rt
        .run_function(SLICE_BEAM, "add", 2, &[make_small(2), make_small(3)])
        .expect("slice:add/2");
    assert_eq!(sum, make_small(5));

    // The module was loaded (and cached) by `run_function`. The boot that `run_function`
    // performs first also loads the whole preloaded set into the cache (contract A.7),
    // so the *non-preloaded* cached set is exactly `slice`.
    assert!(rt.is_booted());
    let loaded: Vec<String> = rt
        .loaded_modules()
        .into_iter()
        .filter(|name| !rt.preloaded().is_preloaded(name))
        .collect();
    assert_eq!(loaded, vec!["slice".to_string()]);
    let module = rt.module("slice").expect("cached");
    assert_eq!(module.atom_name(module.name), Some("slice"));
    assert!(module.export(module.atom_table.lookup("add").expect("add"), 2).is_some());
}

#[test]
fn run_function_computes_the_tenth_fibonacci_number() {
    let mut rt = runtime();
    let fib = rt
        .run_function(SLICE_BEAM, "fib", 1, &[make_small(10)])
        .expect("slice:fib/1");
    assert_eq!(fib, make_small(55));
}

#[test]
fn run_module_takes_a_caller_owned_module_and_atom_arguments() {
    let image = load_file(SLICE_BEAM).expect("load slice.beam");
    let a = make_atom(image.atom_table.lookup("a").expect("the atom a"));
    let mut rt = runtime();
    let mut sink = NullSink;
    let picked = rt
        .run_module(&image, "pick", 1, &[a], &mut sink)
        .expect("slice:pick(a)");
    assert_eq!(picked, make_small(1));

    // The catch-all clause is reached through the module's own atom table too.
    let other = make_atom(image.atom_table.lookup("slice").expect("the atom slice"));
    let picked = rt
        .run_module(&image, "pick", 1, &[other], &mut sink)
        .expect("slice:pick(slice)");
    assert_eq!(picked, make_small(2));
}

#[test]
fn a_missing_export_raises_undef() {
    // The oracle raises `exception error undef` for a call to a function the release does
    // not have; the module/function/arity live in the stacktrace, not the reason.
    let mut rt = runtime();
    let error = rt
        .run_function(SLICE_BEAM, "nope", 0, &[])
        .expect_err("not exported");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
    let error = rt
        .run_function(SLICE_BEAM, "add", 3, &[make_small(1)])
        .expect_err("wrong arity");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
}

#[test]
fn results_are_printable_by_the_registered_printer() {
    // The result of a run lives on the runtime's process heap, so it survives both
    // the run and the printing.
    let mut rt = runtime();
    let sum = rt
        .run_function(SLICE_BEAM, "add", 2, &[make_small(2), make_small(3)])
        .expect("slice:add/2");
    assert_eq!(rt.format_term(sum, None).expect("printable"), "5\n");
    let fib = rt
        .run_function(SLICE_BEAM, "fib", 1, &[make_small(10)])
        .expect("slice:fib/1");
    assert_eq!(rt.format_term(fib, None).expect("printable"), "55\n");
}

#[test]
fn system_info_round_trips_through_the_runtime() {
    let mut rt = runtime();
    let wordsize = rt.system_info("wordsize").expect("wordsize");
    assert!(is_small(wordsize));
    assert_eq!(signed_value(wordsize), std::mem::size_of::<usize>() as i64);

    let version = rt.system_info("version").expect("version");
    assert!(is_list(version), "`version` is an Erlang string");
    assert_eq!(
        rt.system_info_value("version").expect("version"),
        beam_rt::InfoValue::Str("17.1".to_string())
    );

    assert!(matches!(
        rt.system_info("nonsense"),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn eval_and_run_share_the_same_runtime() {
    let mut rt = runtime();
    let three = rt.eval("1+2").expect("1+2");
    assert_eq!(three, make_small(3));
    let sum: Eterm = rt
        .run_function(SLICE_BEAM, "add", 2, &[three, make_small(4)])
        .expect("slice:add/2");
    assert_eq!(sum, make_small(7));
}
