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

//! Tier-equivalence and structural pins for the three BIF fast paths
//! (`super::emit::bif`: `erlang:hd/1`, `erlang:element/2`, `erlang:tuple_size/1`).
//! A `#[path]` child of `super::tests`, so it shares that module's harness
//! (`build_module`, `export_atom`, `run_counting`, `assert_same_outcome`).
//!
//! **Why these tests install their own BIF host.** On the **interpreter** tier a
//! `{bif,…}` instruction *always* reaches the host through `call_bif`; only the baseline
//! tier's fast path computes the value natively. A host that rejects every BIF
//! (`MinimalContext::new`, what [`super::with_both_tiers`] builds) would therefore make the
//! two tiers disagree on a fast-path shape — the interpreter raising, the baseline
//! returning — so the equivalence tests here install [`bif_host`], a hook implementing
//! exactly these three BIFs with the real `erlang` semantics (and rejecting everything
//! else). What is then pinned is the property that matters: the fast path returns **the
//! same value the BIF does**, and every declined shape is the interpreter's own typed
//! `badarg`.
//!
//! Each BIF is covered three ways:
//!
//! * a **fast-path** case, asserting the concrete expected value on **both** tiers (not
//!   merely that they agree) plus [`assert_same_outcome`];
//! * **declined** shapes — where the tier refuses the shape at compile time or the run-time
//!   guard rejects — which must be the interpreter's exact `badarg` on both tiers;
//! * a **structural pin** that the fast-path shape no longer takes the `emit_deopt` path
//!   (the emitter's own `instr_deopts` decision on a **cache-free** compile, plus the run's
//!   per-variant deopt attribution and its zero interpreter steps).

use super::emit::compile_function;
use super::*;use crate::context::BifHook;
use crate::terms::{cons_head, tuple_arity, tuple_element};
use beam_term::{is_small, make_atom, signed_value};

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

/// The host whose BIF hook implements the three BIFs under test with the real `erlang`
/// semantics: `hd/1` is the cons head (`badarg` otherwise), `element/2` is the 1-based
/// tuple element (`badarg` for a non-integer or out-of-range index, or a non-tuple) and
/// `tuple_size/1` is the tuple's arity (`badarg` otherwise). Every other BIF is rejected,
/// exactly like [`MinimalContext::new`].
fn bif_host() -> BifHook {
    Box::new(|module, function, arity, args| match (module, function, arity) {
        ("erlang", "hd", 1) => {
            let [source] = args else {
                return Err(beam_common::BeamError::BadArg);
            };
            cons_head(*source).map_err(|_| beam_common::BeamError::BadArg)
        }
        ("erlang", "element", 2) => {
            let [index, tuple] = args else {
                return Err(beam_common::BeamError::BadArg);
            };
            if !is_small(*index) {
                return Err(beam_common::BeamError::BadArg);
            }
            let arity = tuple_arity(*tuple).map_err(|_| beam_common::BeamError::BadArg)?;
            let index = signed_value(*index);
            if index < 1 || index as usize > arity {
                return Err(beam_common::BeamError::BadArg);
            }
            tuple_element(*tuple, index as usize - 1)
                .map_err(|_| beam_common::BeamError::BadArg)
        }
        ("erlang", "tuple_size", 1) => {
            let [tuple] = args else {
                return Err(beam_common::BeamError::BadArg);
            };
            let arity = tuple_arity(*tuple).map_err(|_| beam_common::BeamError::BadArg)?;
            Ok(make_small(arity as i64))
        }
        _ => Err(beam_common::BeamError::BadArg),
    })
}

/// Run exported `name/0` on both tiers with [`bif_host`] installed and hand the two
/// outcomes to `check` — the hooked sibling of [`super::with_both_tiers`], needed because
/// the interpreter tier reaches these three BIFs through `call_bif` (see the module docs).
///
/// As in its sibling, each run builds its boxed terms on its **own** [`MinimalContext`]
/// heap, so the comparison must happen inside this call.
fn with_both_tiers_bif_host<R>(
    module: &Module,
    name: Atom,
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::with_bif_hook(bif_host());
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        0,
        &[],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::with_bif_hook(bif_host());
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        0,
        &[],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

// ---------------------------------------------------------------------------
// the modules
// ---------------------------------------------------------------------------

/// A one-export module (`f/0`) with a single `erlang:M/A` import, so every case's
/// `Instr::Bif` below targets `ImportRef(0)`.
fn bif_module(name: &str, function: &str, arity: u32, body: Vec<Instr>) -> (Module, Atom) {
    let module = build_module(
        name,
        &[("erlang", function, arity)],
        vec![("f", 0, body)],
        &[("f", 0, Label(2))],
    );
    let export = export_atom(&module, "f");
    (module, export)
}

/// The non-collecting `{bif,…}` every case ends its body with.
fn bif_call(arity: u8, args: Vec<Operand>, dst: Operand) -> Instr {
    Instr::Bif {
        arity,
        bif: ImportRef(0),
        fail: NO_FAIL,
        args,
        dst,
    }
}

/// Replace the `PutTuple2` at `pc` of the module's single function with one building the
/// tuple `names` — `build_module` interns only the module/function/import names, so an
/// element atom has to be added to the table after the module exists.
fn with_interned_tuple(module: &mut Module, pc: usize, names: &[&str]) {
    let elements = names
        .iter()
        .map(|name| Operand::Atom(module.atom_table.intern(name)))
        .collect();
    module.functions[0].code[pc] = Instr::PutTuple2 {
        dst: Operand::X(0),
        elements,
    };
}

/// `hd([7, 8])` — the fast-path shape: a cons cell in `x(0)`.
fn hd_cons_case() -> (Module, Atom) {
    bif_module(
        "bif_hd_cons",
        "hd",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::PutList {
                head: Operand::Integer(8),
                tail: Operand::Nil,
                dst: Operand::X(1),
            },
            Instr::PutList {
                head: Operand::Integer(7),
                tail: Operand::X(1),
                dst: Operand::X(0),
            },
            bif_call(1, vec![Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    )
}

/// `hd(Value)` where `Value` is not a cons cell: the `is_list` guard rejects and the
/// interpreter's own `hd/1` raises `badarg`.
fn hd_declined_case(name: &str, value: Operand) -> (Module, Atom) {
    bif_module(
        name,
        "hd",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Move {
                src: value,
                dst: Operand::X(0),
            },
            bif_call(1, vec![Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    )
}

/// `element(2, {a, b, c})` — the fast-path shape: an immediate index and a tuple.
fn element_tuple_case() -> (Module, Atom) {
    let (mut module, export) = bif_module(
        "bif_element_tuple",
        "element",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            // Patched by `with_interned_tuple` below.
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: Vec::new(),
            },
            bif_call(2, vec![Operand::Integer(2), Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    );
    with_interned_tuple(&mut module, 2, &["a", "b", "c"]);
    (module, export)
}

/// `element(Index, {1})` with the given index operand: an index below `1` is refused at
/// compile time and one above the tuple's arity is rejected by the run-time guard, so both
/// reach the interpreter's own `element/2` (`badarg`).
fn element_index_declined_case(name: &str, index: Operand) -> (Module, Atom) {
    bif_module(
        name,
        "element",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::Integer(1)],
            },
            bif_call(2, vec![index, Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    )
}

/// `element(1, 5)` — the inlined shape's `is_tuple` guard rejects a non-tuple source.
fn element_non_tuple_case() -> (Module, Atom) {
    bif_module(
        "bif_element_non_tuple",
        "element",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(5),
                dst: Operand::X(0),
            },
            bif_call(2, vec![Operand::Integer(1), Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    )
}

/// `tuple_size({a, b})` — the fast-path shape (the arity is read out of the header word).
fn tuple_size_tuple_case() -> (Module, Atom) {
    let (mut module, export) = bif_module(
        "bif_tuple_size",
        "tuple_size",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            // Patched by `with_interned_tuple` below.
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: Vec::new(),
            },
            bif_call(1, vec![Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    );
    with_interned_tuple(&mut module, 2, &["a", "b"]);
    (module, export)
}

/// `tuple_size([1])` — the inlined shape's `is_tuple` guard rejects a cons cell.
fn tuple_size_cons_case() -> (Module, Atom) {
    bif_module(
        "bif_tuple_size_cons",
        "tuple_size",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::PutList {
                head: Operand::Integer(1),
                tail: Operand::Nil,
                dst: Operand::X(0),
            },
            bif_call(1, vec![Operand::X(0)], Operand::X(0)),
            Instr::Return,
        ],
    )
}

/// Assert both tiers returned `expected`, then that they agree.
fn assert_value(label: &str, expected: Eterm, outcome: &Result<RunOutcome>) {
    assert_eq!(
        outcome.as_ref().expect("the run completes"),
        &RunOutcome::Returned(expected),
        "{label}: the lowered BIF must produce the BIF's own value"
    );
}
// ---------------------------------------------------------------------------
// (a) the fast paths produce the BIF's value
// ---------------------------------------------------------------------------

#[test]
fn hd_is_lowered_to_the_cons_read() {
    let (module, name) = hd_cons_case();
    with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
        assert_value("hd/1 (interpreter tier)", make_small(7), interpreter);
        assert_value("hd/1 (baseline tier)", make_small(7), baseline);
        assert_same_outcome("hd([7,8])", interpreter, baseline, &module.atom_table);
    });
}

#[test]
fn element_is_lowered_to_the_tuple_read() {
    let (module, name) = element_tuple_case();
    let expected = make_atom(module.atom_table.lookup("b").expect("`b` is interned"));
    with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
        assert_value(
            "element/2 (interpreter tier)",
            expected,
            interpreter,
        );
        assert_value("element/2 (baseline tier)", expected, baseline);
        assert_same_outcome(
            "element(2,{a,b,c})",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

#[test]
fn tuple_size_is_lowered_to_the_tuple_header_read() {
    let (module, name) = tuple_size_tuple_case();
    with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
        assert_value("tuple_size/1 (interpreter tier)", make_small(2), interpreter);
        assert_value("tuple_size/1 (baseline tier)", make_small(2), baseline);
        assert_same_outcome(
            "tuple_size({a,b})",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

// ---------------------------------------------------------------------------
// (b) every declined shape is the interpreter's own `badarg`
// ---------------------------------------------------------------------------

/// The declined shapes — a compile-time refusal (an index below `1`, a non-addressable
/// operand) and a run-time guard rejection (`hd` of a non-cons, an out-of-range or
/// non-integer index, a non-tuple source) — must all reach the interpreter's step and
/// raise its exact `badarg`, identically on both tiers. A fast path that wrongly accepted
/// any of them would return a wrong term (or panic) instead.
#[test]
fn the_declined_bif_shapes_raise_the_interpreters_badarg() {
    let cases = [
        ("hd([])", hd_declined_case("bif_hd_nil", Operand::Nil)),
        ("hd(5)", hd_declined_case("bif_hd_int", Operand::Integer(5))),
        (
            "element(0,{1})",
            element_index_declined_case("bif_element_index0", Operand::Integer(0)),
        ),
        (
            "element(9,{1})",
            element_index_declined_case("bif_element_index9", Operand::Integer(9)),
        ),
        ("element(1,5)", element_non_tuple_case()),
        ("tuple_size([1])", tuple_size_cons_case()),
    ];
    for (label, (module, name)) in cases {
        with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                assert!(
                    matches!(outcome, Err(beam_common::BeamError::BadArg)),
                    "{label} ({tier}): a declined shape must raise the interpreter's \
                     `badarg`; saw {outcome:?}"
                );
            }
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }
}

// ---------------------------------------------------------------------------
// (c) the static deopt is gone
// ---------------------------------------------------------------------------

/// **Each fast-path `Bif` no longer costs a per-instruction interpreter step** — the
/// structural pin, in its two halves, for all three BIFs.
///
/// The static half is measured on a **cache-free** compile ([`compile_function`] lowers the
/// function straight into its own buffer), so it can never observe a thread-local
/// `CodeCache` hit — a hit re-uses an already-emitted artifact and would record nothing,
/// making the pin vacuous rather than red. It pins that the module's `Bif` was **visited**
/// by the emitter yet did **not** take the `emit_deopt` path (`counters::variant_name` of
/// an `Instr::Bif` is `"Bif"`).
///
/// The run-time half drives each module on the baseline tier with the counters recording:
/// the whole fixture is lowered — the `put_list`/`put_tuple2` constructors (the family
/// helper), the `Bif`, and the `return` — so the run must pay **zero** interpreter steps
/// and must not attribute a single deopt to `Bif`.
#[test]
fn the_lowered_bif_fast_paths_record_no_static_deopt() {
    let cases = [
        ("hd/1", hd_cons_case()),
        ("element/2", element_tuple_case()),
        ("tuple_size/1", tuple_size_tuple_case()),
    ];
    for (label, (module, name)) in cases {
        // (1) The emitter's dispatch decision, on a compile that cannot hit the cache.
        super::counters::reset();
        super::counters::set_enabled(true);
        let labels = crate::interpreter::build_label_map(&module);
        let _function = compile_function(&module, &labels, 0).expect("f/0 compiles");
        super::counters::set_enabled(false);
        let emitted = super::counters::snapshot();
        assert!(
            emitted.instr_instances.get("Bif").copied().unwrap_or(0) > 0,
            "{label}: `Bif` must have been visited by the emitter, or this pin proves \
             nothing; instances: {:?}",
            emitted.instr_instances,
        );
        assert_eq!(
            emitted.instr_deopts.get("Bif").copied().unwrap_or(0),
            0,
            "{label}: `Bif` is lowered, so it must record zero static deopts; deopts: {:?}",
            emitted.instr_deopts,
        );

        // (2) The run: the value arrives and no deopt may be attributed to the `Bif`.
        let (outcome, snapshot) = run_counting(&module, name, Tier::BaselineJit);
        assert!(
            matches!(outcome, Ok(RunOutcome::Returned(_))),
            "{label}: the lowered fixture must still return the BIF's value; saw {outcome:?}"
        );
        assert!(
            !snapshot.deopts_by_variant.contains_key("Bif"),
            "{label}: `Bif` must be lowered natively, but it deopted {} time(s) at run \
             time; deopts by variant: {:?}",
            snapshot.deopts_by_variant.get("Bif").copied().unwrap_or(0),
            snapshot.deopts_by_variant,
        );
        assert_eq!(
            snapshot.step_one, 0,
            "{label}: every instruction of this fixture is lowered, so no interpreter step \
             may remain; deopts by variant: {:?}",
            snapshot.deopts_by_variant,
        );
    }
}
