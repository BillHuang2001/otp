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

//! `erlang:trace_pattern/2,3` (contract A.8, item 1) end to end through the runtime.
//!
//! `erts_internal:trace_pattern/3` is the body `erlang:trace_pattern/2,3` calls
//! (`erts/preloaded/src/erlang.erl`'s wrapper catches the BIF's `badarg` and turns it into
//! `{error, badarg}`); this runtime intercepts the `erlang` namespace in `call_bif` before any
//! module is loaded, so every call below goes straight to the BIF — the same dispatch compiled
//! code and the `erlang` wrapper reach.
//!
//! # What is pinned
//!
//! * the **count** — `trace_pattern` answers the number of *exported* functions of the loaded
//!   modules the argument pattern matches (the C's return value), so `{slice, add, 2}` on the
//!   fixture module is `1` and a valid-but-unmatched pattern is `0`, never a raise;
//! * every **wildcard** shape — `{'_', F, A}`, `{M, '_', A}`, `{M, F, '_'}` and the full
//!   wildcard — including that a private function (`fib/3`, which `slice` does not export) is
//!   never counted;
//! * that `true` **installs** an entry in the runtime's pattern table (with its flags and its
//!   `MatchSpec`), and that `false` and `[]` **remove** it;
//! * the `badarg` rejections: a malformed `MFA`, a malformed `MatchSpec`, and a `FlagList`
//!   holding anything outside the seven `trace_pattern` flags — `[call]` is the
//!   oracle-observed rejection.
//!
//! # What is deliberately not pinned
//!
//! No `{trace, Pid, call, …}` event is asserted, because none is emitted: nothing in this
//! runtime is wired into the engine's call/return paths yet, so an installed pattern is state
//! only. `PatternTable::matches` — exercised through `Runtime::trace_patterns` — is the
//! predicate a future `beam_engine` per-call hook consults to decide whether to emit one.
use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{signed_value, Eterm, NIL};

/// The committed fixture module the loader/engine suites share: its exports are `add/2`,
/// `fib/1`, `pick/1`, `module_info/0` and `module_info/1` (the private `fib/3` is not one).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// A `Runtime` with `slice.beam` loaded — the one module every count below is computed over.
fn runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(SLICE_BEAM).expect("slice.beam loads");
    rt
}

/// Run `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are dropped
/// before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// `erlang:trace_pattern(MFA, MatchSpec, FlagList)` through the BIF dispatch.
fn trace_pattern_3(rt: &mut Runtime, args: &[Eterm]) -> Result<Eterm, BeamError> {
    with_context(rt, |ctx| ctx.call_bif("erlang", "trace_pattern", 3, args))
}

/// `erlang:trace_pattern(MFA, MatchSpec)` through the BIF dispatch.
fn trace_pattern_2(rt: &mut Runtime, args: &[Eterm]) -> Result<Eterm, BeamError> {
    with_context(rt, |ctx| ctx.call_bif("erlang", "trace_pattern", 2, args))
}

/// The integer count a successful `trace_pattern` answers, panicking with the runtime's own
/// reason when it raises.
fn count(result: Result<Eterm, BeamError>) -> i64 {
    signed_value(result.unwrap_or_else(|error| panic!("trace_pattern failed: {error:?}")))
}

/// Assert `error` is the oracle's plain `badarg` (a bare reason atom, never a tuple): a
/// non-atom reason still fails this loudly.
fn assert_badarg(error: BeamError) {
    assert_eq!(
        error.as_atom(),
        Some("badarg"),
        "expected error:badarg, got {error:?}"
    );
}

/// The fixture runtime loads exactly the one module this suite reasons about, so the counts
/// below are exact numbers rather than "at least" bounds.
#[test]
fn only_the_fixture_module_is_loaded() {
    let rt = runtime();
    assert_eq!(
        rt.loaded_modules(),
        vec!["slice".to_string()],
        "the count assertions assume `slice` is the only loaded module"
    );
}

/// `erlang:trace_pattern(MFA, true, [local])` answers the number of exported functions the
/// pattern matches, for every wildcard shape.
#[test]
fn the_count_is_the_number_of_exported_functions_matched() {
    let mut rt = runtime();
    let ms_true = rt.intern("true");
    let flags_local = rt.eval("[local]").expect("[local]");

    // A concrete export: `slice:add/2`.
    let mfa = rt.eval("{slice, add, 2}").expect("{slice, add, 2}");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 1);

    // `{M, '_', A}`: every function of arity 1 in the module (fib/1, pick/1, module_info/1).
    let mfa = rt.eval("{slice, '_', 1}").expect("{slice, '_', 1}");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 3);

    // `{M, F, '_'}`: every arity a function is exported with. `fib` is exported as `fib/1`
    // only — the private `fib/3` is not in the export table.
    let mfa = rt.eval("{slice, fib, '_'}").expect("{slice, fib, '_'}");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 1);

    let mfa = rt.eval("{slice, module_info, '_'}").expect("{slice, module_info, '_'}");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 2);

    // `{'_', F, A}`: every module. The runtime has loaded only the fixture.
    let mfa = rt.eval("{'_', add, 2}").expect("{'_', add, 2}");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 1);

    // The full wildcard is every export of every loaded module (`slice` has five).
    let mfa = rt.eval("{'_', '_', '_'}").expect("the full wildcard");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 5);

    // A valid but unmatched pattern is `0`, never a raise: an unknown function name, an arity
    // no export has, and a module that is not loaded.
    for mfa in [
        rt.eval("{slice, undefined, 0}").expect("an unknown function"),
        rt.eval("{slice, add, 9}").expect("an arity no export has"),
        rt.eval("{no_such_module, add, 2}").expect("an unloaded module"),
        rt.eval("{'_', no_such_function, 0}").expect("no such function anywhere"),
    ] {
        assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 0);
    }
}

/// `true` installs an entry in the runtime's pattern table (flags and match spec included),
/// and the arity-2 form is the arity-3 form with an empty `FlagList`.
#[test]
fn true_installs_a_pattern_with_its_flags_and_match_spec() {
    use beam_rt::trace::{pattern_flags_to_atoms, MatchSpec};

    let mut rt = runtime();
    let mfa = rt.eval("{slice, add, 2}").expect("{slice, add, 2}");
    let ms_true = rt.intern("true");
    let flags_local = rt.eval("[local]").expect("[local]");

    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 1);
    let entry = rt
        .trace_patterns()
        .matches("slice", "add", 2)
        .expect("the pattern is installed");
    assert_eq!(pattern_flags_to_atoms(entry.flags), vec!["local"]);
    assert_eq!(entry.match_spec, MatchSpec::MatchAll);
    // Only the selected MFA is governed; a different arity is not.
    assert!(rt.trace_patterns().matches("slice", "add", 3).is_none());

    // A `[match_spec()]` is retained verbatim (never compiled) in place of the catch-all.
    let ms_list = rt.eval("[{a, b}]").expect("[{a, b}]");
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_list, flags_local])), 1);
    let entry = rt
        .trace_patterns()
        .matches("slice", "add", 2)
        .expect("still installed once");
    match &entry.match_spec {
        MatchSpec::Functions(functions) => {
            assert_eq!(functions.len(), 1, "the one match function was stored");
            assert_eq!(terms::tuple_arity(functions[0]).expect("a tuple"), 2);
        }
        other => panic!("expected the stored match functions, got {other:?}"),
    }
    // Re-installing the *same* pattern replaces it in place rather than duplicating it.
    assert_eq!(rt.trace_patterns().len(), 1);

    // The arity-2 form installs with an empty flag list.
    let mfa = rt.eval("{slice, fib, 1}").expect("{slice, fib, 1}");
    assert_eq!(count(trace_pattern_2(&mut rt, &[mfa, ms_true])), 1);
    let entry = rt
        .trace_patterns()
        .matches("slice", "fib", 1)
        .expect("the /2 form installed it");
    assert!(pattern_flags_to_atoms(entry.flags).is_empty(), "no flags were given");
    assert_eq!(rt.trace_patterns().len(), 2, "two distinct patterns are installed");
}

/// `false` and `[]` remove an installed pattern — and still answer the count of matched
/// functions, because that answer depends on the argument pattern alone.
#[test]
fn false_and_the_empty_list_remove_an_installed_pattern() {
    let mut rt = runtime();
    let mfa = rt.eval("{slice, add, 2}").expect("{slice, add, 2}");
    let ms_true = rt.intern("true");
    let ms_false = rt.intern("false");
    let flags_local = rt.eval("[local]").expect("[local]");

    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 1);
    assert!(rt.trace_patterns().matches("slice", "add", 2).is_some());

    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_false, flags_local])), 1);
    assert!(
        rt.trace_patterns().matches("slice", "add", 2).is_none(),
        "`false` removed the pattern"
    );

    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_true, flags_local])), 1);
    assert!(rt.trace_patterns().matches("slice", "add", 2).is_some());
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, NIL, flags_local])), 1);
    assert!(
        rt.trace_patterns().matches("slice", "add", 2).is_none(),
        "`[]` removed the pattern"
    );

    // Removing a pattern that was never installed is a successful no-op, not an error.
    assert_eq!(count(trace_pattern_3(&mut rt, &[mfa, ms_false, flags_local])), 1);
    assert!(rt.trace_patterns().is_empty());
}

/// The `badarg` rejections, grouped by which argument is wrong. All of them are the plain
/// `badarg` the `erlang.erl` wrapper turns into `{error, badarg}` — never a raised tuple from
/// this runtime, and never a wrong count.
#[test]
fn the_badarg_paths() {
    let mut rt = runtime();
    let flags_local = rt.eval("[local]").expect("[local]");
    let ms_true = rt.intern("true");
    let mfa = rt.eval("{slice, add, 2}").expect("{slice, add, 2}");

    // A malformed `MFA`: not a tuple, the wrong tuple arity, a non-atom name, and an arity
    // that is neither a non-negative small integer nor `'_'`.
    let not_a_tuple = rt.intern("foo");
    let short_tuple = rt.eval("{slice, add}").expect("{slice, add}");
    let four_tuple = rt.eval("{slice, add, 2, 3}").expect("{slice, add, 2, 3}");
    let integer_name = rt.eval("{1, add, 2}").expect("{1, add, 2}");
    let atom_arity = rt.eval("{slice, add, foo}").expect("{slice, add, foo}");
    let negative_arity = rt.eval("{slice, add, -1}").expect("{slice, add, -1}");
    for bad in [
        not_a_tuple,
        short_tuple,
        four_tuple,
        integer_name,
        atom_arity,
        negative_arity,
    ] {
        assert_badarg(
            trace_pattern_3(&mut rt, &[bad, ms_true, flags_local])
                .expect_err("a malformed MFA is badarg"),
        );
    }

    // A malformed `MatchSpec`: another atom, a non-list, a list element that is not a
    // two-or-three-tuple match function.
    let other_atom = rt.intern("maybe");
    let an_integer = rt.eval("5").expect("5");
    let non_tuple_element = rt.eval("[5]").expect("[5]");
    let one_tuple_element = rt.eval("[{a}]").expect("[{a}]");
    for bad in [other_atom, an_integer, non_tuple_element, one_tuple_element] {
        assert_badarg(
            trace_pattern_3(&mut rt, &[mfa, bad, flags_local])
                .expect_err("a malformed MatchSpec is badarg"),
        );
    }

    // A malformed `FlagList`: not a list, an atom outside the seven `trace_pattern` flags —
    // `call` is the oracle-observed rejection (it is a *process* trace flag) — and a non-atom
    // element.
    let call = rt.eval("[call]").expect("[call]");
    let unknown = rt.eval("[not_a_pattern_flag]").expect("[not_a_pattern_flag]");
    let non_atom_element = rt.eval("[5]").expect("[5]");
    let not_a_list = rt.intern("local");
    for bad in [call, unknown, non_atom_element, not_a_list] {
        assert_badarg(
            trace_pattern_3(&mut rt, &[mfa, ms_true, bad])
                .expect_err("a malformed FlagList is badarg"),
        );
    }

    // Nothing was installed by any rejected call.
    assert!(rt.trace_patterns().is_empty());
}

/// `erts_internal:trace_pattern/3` is the BIF `erlang:trace_pattern/2,3` delegates to, so it
/// answers the same count and installs the same entry.
#[test]
fn the_erts_internal_body_is_the_same_bif() {
    let mut rt = runtime();
    let mfa = rt.eval("{slice, pick, 1}").expect("{slice, pick, 1}");
    let ms_true = rt.intern("true");
    let flags_meta = rt.eval("[meta]").expect("[meta]");

    let result = with_context(&mut rt, |ctx| {
        ctx.call_bif("erts_internal", "trace_pattern", 3, &[mfa, ms_true, flags_meta])
    });
    assert_eq!(count(result), 1);
    assert_eq!(
        beam_rt::trace::pattern_flags_to_atoms(
            rt.trace_patterns()
                .matches("slice", "pick", 1)
                .expect("installed")
                .flags
        ),
        vec!["meta"]
    );
}
