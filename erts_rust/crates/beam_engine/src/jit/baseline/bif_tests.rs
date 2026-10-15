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

//! Tier-equivalence and structural pins for the lowered `Bif`/`GcBif` fast paths:
//! the three non-collecting `Instr::Bif`s (`super::emit::bif`: `erlang:hd/1`,
//! `erlang:element/2`, `erlang:tuple_size/1`) and the three collecting
//! `Instr::GcBif`s (`super::emit::arith`: `erlang:band/2`, `erlang:bor/2`,
//! `erlang:bsr/2` — each can yield a bignum, so the compiler emits the collecting
//! form).
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
        ("erlang", "band", 2) => {
            let (a, b) = small_pair(args)?;
            Ok(make_small(a & b))
        }
        ("erlang", "bor", 2) => {
            let (a, b) = small_pair(args)?;
            Ok(make_small(a | b))
        }
        ("erlang", "bsr", 2) => bsr_small(args),
        _ => Err(beam_common::BeamError::BadArg),
    })
}

/// The small-int domain of `band`/`bor`/`bsr`: both operands must be **small
/// integers**, and anything else is the `badarith` the real BIF raises (the oracle's
/// `band(1.0, 2)` and `band(a, 2)` are both `badarith`, `erl_arith.c`'s
/// `integer`/`badarith`).
///
/// A **bignum** operand is outside this host's scope: a `BifHook` receives no
/// allocator, so it cannot materialize one, and the fixtures cannot build one either.
/// That is harmless for the property under test — the tier's guards reject *every*
/// non-small word (`(word & 0xF) != 0xF`), so a bignum and an atom reach the same
/// compile-time refusal / run-time rejection.
fn small_pair(args: &[Eterm]) -> Result<(i64, i64)> {
    let [a, b] = args else {
        return Err(beam_common::BeamError::BadArith);
    };
    if is_small(*a) && is_small(*b) {
        Ok((signed_value(*a), signed_value(*b)))
    } else {
        Err(beam_common::BeamError::BadArith)
    }
}

/// `erlang:bsr/2` for the small-int domain — `erts_shift` with `left == false`: `A
/// bsr N` is `A >> N` sign-extending, a negative `N` is `A bsl -N`, and a right shift
/// past the word width keeps only the operand's sign (`0`/`-1`).
///
/// A `bsl` whose exact result leaves the small range is beyond the hook (it cannot
/// allocate the bignum), so it reports the typed `system_limit` instead of a
/// fabricated small; no fixture reaches it.
fn bsr_small(args: &[Eterm]) -> Result<Eterm> {
    let (a, n) = small_pair(args)?;
    let value = if n >= 0 {
        if n >= 64 {
            if a < 0 {
                -1
            } else {
                0
            }
        } else {
            a >> (n as u32)
        }
    } else {
        let shift = n.unsigned_abs();
        if shift >= 64 {
            // `A bsl <huge>`: `0` for a zero operand, else `system_limit`.
            if a == 0 {
                0
            } else {
                return Err(beam_common::BeamError::SystemLimit);
            }
        } else {
            // Exact `A bsl shift`; a result outside the small range is a bignum,
            // which this hook cannot build (see the doc above).
            i64::try_from(i128::from(a) * (1i128 << shift))
                .map_err(|_| beam_common::BeamError::SystemLimit)?
        }
    };
    Ok(make_small(value))
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

// ---------------------------------------------------------------------------
// (d) the Y-operand shapes keep the published frame window
// ---------------------------------------------------------------------------

/// `hd(Y(0))` where `Y(0)` holds the cons cell — the lowered `Bif`'s **`Y` source**
/// operand, read through the guarded frame window.
fn hd_y_source_case() -> (Module, Atom) {
    bif_module(
        "bif_hd_y_source",
        "hd",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(1),
                live: 0,
            },
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
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(0),
            },
            bif_call(1, vec![Operand::Y(0)], Operand::X(0)),
            Instr::Return,
        ],
    )
}

/// `tuple_size({a, b})` with the destination in `Y(0)` — the lowered `Bif`'s **`Y`
/// destination**, whose frame-window guard is emitted before the value is computed.
fn tuple_size_y_destination_case() -> (Module, Atom) {
    let (mut module, export) = bif_module(
        "bif_tuple_size_y",
        "tuple_size",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(1),
                live: 1,
            },
            // Patched by `with_interned_tuple` below.
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: Vec::new(),
            },
            bif_call(1, vec![Operand::X(0)], Operand::Y(0)),
            Instr::Move {
                src: Operand::Y(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    with_interned_tuple(&mut module, 3, &["a", "b"]);
    (module, export)
}

/// **A `Y` operand of a lowered `Bif` is served by the guarded frame window**, not by a
/// stale `X` slot: the syntactic gate [`uses_y`] reports the instruction (so the block
/// publishes/refreshes the window), and both tiers return the BIF's value on **both**
/// operand sides — a `Y` source for `hd/1` and a `Y` destination for `tuple_size/1`.
///
/// A lowering that ignored `Y` would read/write the wrong slot (or a null window) and
/// diverge from the interpreter; the `Y`-destination case additionally pins that the
/// value is stored into the slot the following instruction reads, i.e. that the
/// destination guard is emitted before the value is computed.
#[test]
fn the_y_operand_shapes_are_lowered_and_agree_across_the_tiers() {
    let (hd_module, hd_name) = hd_y_source_case();
    let (size_module, size_name) = tuple_size_y_destination_case();
    assert!(
        uses_y(&hd_module, &hd_module.functions[0].code[6]),
        "the `Bif` of `hd(Y(0))` reads the frame window, so the block must publish it"
    );
    assert!(
        uses_y(&size_module, &size_module.functions[0].code[4]),
        "the `Bif` of `tuple_size(T) -> Y(0)` writes the frame window, so the block must \
         publish it"
    );

    with_both_tiers_bif_host(&hd_module, hd_name, |interpreter, baseline| {
        assert_value("hd(Y(0)) (interpreter tier)", make_small(7), interpreter);
        assert_value("hd(Y(0)) (baseline tier)", make_small(7), baseline);
        assert_same_outcome(
            "hd of a cons in a Y slot",
            interpreter,
            baseline,
            &hd_module.atom_table,
        );
    });
    with_both_tiers_bif_host(&size_module, size_name, |interpreter, baseline| {
        assert_value(
            "tuple_size(T) into Y(0) (interpreter tier)",
            make_small(2),
            interpreter,
        );
        assert_value(
            "tuple_size(T) into Y(0) (baseline tier)",
            make_small(2),
            baseline,
        );
        assert_same_outcome(
            "tuple_size into a Y slot",
            interpreter,
            baseline,
            &size_module.atom_table,
        );
    });
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

// ---------------------------------------------------------------------------
// (e) the collecting `gc_bif` fast paths: `erlang:band/2`, `erlang:bor/2`, `erlang:bsr/2`
// ---------------------------------------------------------------------------
//
// These three arrive as **`Instr::GcBif`** (each can yield a bignum, so the compiler
// emits the collecting form) and are lowered by the arithmetic fast path
// (`super::emit::arith`: a raw tagged-word `and`/`or` for the bitwise pair, and an
// untag → arithmetic shift by a compile-time count → re-tag for `bsr`).

/// The non-collecting `{gc_bif,…}` every case in this section ends its body with.
fn gc_bif_call(args: Vec<Operand>, dst: Operand) -> Instr {
    Instr::GcBif {
        arity: args.len() as u8,
        bif: ImportRef(0),
        fail: NO_FAIL,
        live: args.len() as u32,
        args,
        dst,
    }
}

/// A one-export (`f/0`) module whose single `{gc_bif,…}` calls `erlang:function/2`
/// with `args`, writing `x(0)`, preceded by `prologue` — the way a fixture puts a
/// non-small word into a register for the run-time guard.
fn gc_arith_module(
    name: &str,
    function: &str,
    prologue: Vec<Instr>,
    args: Vec<Operand>,
) -> (Module, Atom) {
    let mut body = vec![Instr::Label(Label(1)), Instr::Label(Label(2))];
    body.extend(prologue);
    body.push(gc_bif_call(args, Operand::X(0)));
    body.push(Instr::Return);
    bif_module(name, function, 2, body)
}

/// `erlang:function/2` over the two given operands, with no prologue.
fn arith_module(name: &str, function: &str, a: Operand, b: Operand) -> (Module, Atom) {
    gc_arith_module(name, function, Vec::new(), vec![a, b])
}

/// `function(A, B)` where both operands are registers holding small constants — the
/// register-operand (non-immediate) shape of the fast path.
fn arith_registers_module(
    name: &str,
    function: &str,
    a: i64,
    b: i64,
) -> (Module, Atom) {
    gc_arith_module(
        name,
        function,
        vec![
            Instr::Move {
                src: Operand::Integer(a),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(b),
                dst: Operand::X(1),
            },
        ],
        vec![Operand::X(0), Operand::X(1)],
    )
}

/// `function(args)` where x-register `slot` holds a **non-small** word: the prologue
/// move is patched to an atom interned *in this module's own table* (atom ids are per
/// table), so the shape matches at compile time and the emitted `is_small` guard
/// rejects at run time — falling back to one interpreter step.
fn arith_guarded_module(
    name: &str,
    function: &str,
    slot: u32,
    args: Vec<Operand>,
) -> (Module, Atom) {
    let (mut module, export) = gc_arith_module(
        name,
        function,
        vec![Instr::Move {
            src: Operand::Nil,
            dst: Operand::X(slot),
        }],
        args,
    );
    let atom = Operand::Atom(module.atom_table.intern("nope"));
    *guard_move(&mut module, slot) = Instr::Move {
        src: atom,
        dst: Operand::X(slot),
    };
    (module, export)
}

/// `function(atom, B)` — a **non-number constant** operand, refused at compile time
/// (a constant that is not a small integer is not loadable).
fn arith_atom_module(name: &str, function: &str, b: Operand) -> (Module, Atom) {
    let (mut module, export) = arith_module(name, function, Operand::Nil, b);
    let atom = Operand::Atom(module.atom_table.intern("nope"));
    match &mut module.functions[0].code[2] {
        Instr::GcBif { args, .. } => args[0] = atom,
        other => panic!("the fixture's `gc_bif` must be at code[2]; saw {other:?}"),
    }
    (module, export)
}

/// The `Move { dst: X(slot) }` prologue instruction of a [`gc_arith_module`] fixture.
fn guard_move(module: &mut Module, slot: u32) -> &mut Instr {
    module.functions[0]
        .code
        .iter_mut()
        .find(|instr| matches!(instr, Instr::Move { dst: Operand::X(x), .. } if *x == slot))
        .expect("the prologue move is present")
}

// -- (e.1) the fast paths produce the BIF's value ---------------------------

/// The fast-path shapes, each asserted to produce the **BIF's own value** on both
/// tiers: an immediate-operand pair, a negative operand (where the raw tagged-word
/// `and`/`or` must still be exact), a register-operand pair, and a `. `Y`-slot source
/// read through the guarded frame window.
#[test]
fn the_bitwise_and_shift_gc_bifs_are_lowered_to_the_bifs_value() {
    // (label, module, expected value)
    let cases: [(&str, (Module, Atom), i64); 8] = [
        ("band(12,10)", arith_module("bif_band", "band", Operand::Integer(12), Operand::Integer(10)), 8),
        ("band(-1,5)", arith_module("bif_band_neg", "band", Operand::Integer(-1), Operand::Integer(5)), 5),
        ("band(-6,-4)", arith_module("bif_band_neg2", "band", Operand::Integer(-6), Operand::Integer(-4)), -8),
        ("bor(-1,0)", arith_module("bif_bor", "bor", Operand::Integer(-1), Operand::Integer(0)), -1),
        ("bor(12,10)", arith_module("bif_bor2", "bor", Operand::Integer(12), Operand::Integer(10)), 14),
        ("band(X0,X1) registers", arith_registers_module("bif_band_regs", "band", 12, 10), 8),
        ("bsr(256,4)", arith_module("bif_bsr", "bsr", Operand::Integer(256), Operand::Integer(4)), 16),
        ("bsr(-256,4)", arith_module("bif_bsr_neg", "bsr", Operand::Integer(-256), Operand::Integer(4)), -16),
    ];
    for (label, (module, name), expected) in cases {
        with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
            let expected = make_small(expected);
            assert_value(&format!("{label} (interpreter tier)"), expected, interpreter);
            assert_value(&format!("{label} (baseline tier)"), expected, baseline);
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }
}

/// **A `Y`-slot operand of a lowered `gc_bif` is served by the guarded frame
/// window**: the syntactic gate [`uses_y`] reports the instruction (so the block
/// publishes the window) and both tiers return the BIF's value.
#[test]
fn the_bitwise_gc_bif_y_operand_shape_agrees_across_the_tiers() {
    let (module, name) = gc_arith_module(
        "bif_band_y",
        "band",
        vec![
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(1),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(10),
                dst: Operand::Y(0),
            },
        ],
        vec![Operand::Integer(12), Operand::Y(0)],
    );
    assert!(
        uses_y(&module, &module.functions[0].code[4]),
        "the `gc_bif` of `band(12, Y(0))` reads the frame window, so the block must publish it"
    );
    with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
        assert_value("band(12, Y(0)) (interpreter tier)", make_small(8), interpreter);
        assert_value("band(12, Y(0)) (baseline tier)", make_small(8), baseline);
        assert_same_outcome("band of a Y slot", interpreter, baseline, &module.atom_table);
    });
}

/// `bsr(X, 0)` is the operand unchanged (the emitted shift is omitted) and
/// `bsr(X, 63)` is a legal, exactly-representable shift — both on both tiers.
#[test]
fn the_bsr_count_bounds_are_exact_on_both_tiers() {
    let cases = [
        ("bsr(7,0)", arith_module("bif_bsr0", "bsr", Operand::Integer(7), Operand::Integer(0)), 7),
        ("bsr(-7,0)", arith_module("bif_bsr0n", "bsr", Operand::Integer(-7), Operand::Integer(0)), -7),
        ("bsr(7,63)", arith_module("bif_bsr63", "bsr", Operand::Integer(7), Operand::Integer(63)), 0),
        ("bsr(-7,63)", arith_module("bif_bsr63n", "bsr", Operand::Integer(-7), Operand::Integer(63)), -1),
    ];
    for (label, (module, name), expected) in cases {
        with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
            let expected = make_small(expected);
            assert_value(&format!("{label} (interpreter tier)"), expected, interpreter);
            assert_value(&format!("{label} (baseline tier)"), expected, baseline);
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }
}

// -- (e.2) every declined shape is the interpreter's own outcome -----------

/// The shape equivalence of the raw-tagged-word `and`/`or` is checked over a wide
/// operand grid against the **real BIF semantics** (the host) on both tiers: the fast
/// path must never fabricate a value the BIF would not have produced.
///
/// The grid spans the interesting smalls (`0`, `±1`, a spread of bit patterns and the
/// two range ends), and each case asserts the concrete expected value on the
/// interpreter tier and the baseline tier plus [`assert_same_outcome`].
#[test]
fn the_bitwise_fast_path_agrees_with_the_bif_over_a_small_grid() {
    let values = [0i64, 1, -1, 255, -256, MAX_SMALL, MIN_SMALL];
    for (index, (a, b)) in values
        .iter()
        .flat_map(|a| values.iter().map(move |b| (*a, *b)))
        .enumerate()
    {
        for (function, expected) in [("band", a & b), ("bor", a | b)] {
            let (module, name) =
                arith_registers_module(&format!("bif_grid_{function}_{index}"), function, a, b);
            with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
                let expected = make_small(expected);
                let label = format!("{function}({a},{b})");
                assert_value(&format!("{label} (interpreter tier)"), expected, interpreter);
                assert_value(&format!("{label} (baseline tier)"), expected, baseline);
                assert_same_outcome(&label, interpreter, baseline, &module.atom_table);
            });
        }
    }
}

/// The shapes the fast path **declines**, each asserted to be the interpreter's own
/// outcome on both tiers:
///
/// * a **compile-time** refusal — a non-number constant operand (an atom, `nil`), a
///   wide constant (a bignum at load), and, for `bsr`, a register count, a negative
///   count (which is `bsl`) or a count of `64`/more (which x86 would mask);
/// * a **run-time** guard rejection — the operand register holds a non-small word.
///
/// Every `badarith` case is the host's own `badarith`; every value case is the BIF's
/// own value computed by the interpreter (a refused load-time shape is *not*
/// lowered, so no value is ever fabricated here).
#[test]
fn the_declined_bitwise_and_shift_shapes_are_the_interpreters_outcome() {
    // (a) `badarith`: the operand is a non-number (compile-time refusal for the
    // constant form, run-time guard rejection for the register form).
    let badarith = [
        (
            "band(atom,5)",
            arith_atom_module("bif_band_atom", "band", Operand::Integer(5)),
        ),
        (
            "bor(nil,1)",
            arith_module("bif_bor_nil", "bor", Operand::Nil, Operand::Integer(1)),
        ),
        (
            "band(12,X1=atom)",
            arith_guarded_module(
                "bif_band_guard",
                "band",
                1,
                vec![Operand::Integer(12), Operand::X(1)],
            ),
        ),
        (
            "bsr(X0=atom,4)",
            arith_guarded_module(
                "bif_bsr_guard",
                "bsr",
                0,
                vec![Operand::X(0), Operand::Integer(4)],
            ),
        ),
    ];
    for (label, (module, name)) in badarith {
        with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                assert!(
                    matches!(outcome, Err(beam_common::BeamError::BadArith)),
                    "{label} ({tier}): a declined shape must raise the interpreter's \
                     `badarith`; saw {outcome:?}"
                );
            }
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }

    // (b) the BIF's own value: `bsr` counts the fast path refuses (a register count,
    // a negative count = `bsl`, and a count past the word width) reach the
    // interpreter, whose value the fast path would have got *wrong*.
    let values = [
        (
            "bsr(256,X1=4)",
            arith_registers_module("bif_bsr_regcount", "bsr", 256, 4),
            16,
        ),
        (
            "bsr(3,-1)",
            arith_module("bif_bsr_negcount", "bsr", Operand::Integer(3), Operand::Integer(-1)),
            6,
        ),
        (
            "bsr(3,64)",
            arith_module("bif_bsr_wide", "bsr", Operand::Integer(3), Operand::Integer(64)),
            0,
        ),
        (
            "bsr(-3,64)",
            arith_module("bif_bsr_wide_neg", "bsr", Operand::Integer(-3), Operand::Integer(64)),
            -1,
        ),
    ];
    for (label, (module, name), expected) in values {
        with_both_tiers_bif_host(&module, name, |interpreter, baseline| {
            let expected = make_small(expected);
            assert_value(&format!("{label} (interpreter tier)"), expected, interpreter);
            assert_value(&format!("{label} (baseline tier)"), expected, baseline);
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }
}

// -- (e.3) the static deopt is gone ----------------------------------------

/// **The bitwise/shift `gc_bif` fast paths no longer cost a per-instruction
/// interpreter step** — the structural pin of
/// [`the_lowered_bif_fast_paths_record_no_static_deopt`], in its same two halves,
/// for `band`/`bor`/`bsr`.
///
/// These arrive as the **collecting** `Instr::GcBif` (each can yield a bignum), whose
/// `counters::variant_name` is `"GcBif"`.
#[test]
fn the_lowered_bitwise_gc_bifs_record_no_static_deopt() {
    let cases = [
        (
            "band/2",
            arith_module("bif_band_pin", "band", Operand::Integer(12), Operand::Integer(10)),
        ),
        (
            "bor/2",
            arith_module("bif_bor_pin", "bor", Operand::Integer(12), Operand::Integer(10)),
        ),
        (
            "bsr/2",
            arith_module("bif_bsr_pin", "bsr", Operand::Integer(256), Operand::Integer(4)),
        ),
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
            emitted.instr_instances.get("GcBif").copied().unwrap_or(0) > 0,
            "{label}: `GcBif` must have been visited by the emitter, or this pin proves \
             nothing; instances: {:?}",
            emitted.instr_instances,
        );
        assert_eq!(
            emitted.instr_deopts.get("GcBif").copied().unwrap_or(0),
            0,
            "{label}: `GcBif` is lowered, so it must record zero static deopts; deopts: {:?}",
            emitted.instr_deopts,
        );

        // (2) The run: the value arrives natively and no deopt may be attributed to
        // the `GcBif` (the host here rejects every BIF, so a deopt would raise).
        let (outcome, snapshot) = run_counting(&module, name, Tier::BaselineJit);
        assert!(
            matches!(outcome, Ok(RunOutcome::Returned(_))),
            "{label}: the lowered fixture must still return the BIF's value; saw {outcome:?}"
        );
        assert!(
            !snapshot.deopts_by_variant.contains_key("GcBif"),
            "{label}: `GcBif` must be lowered natively, but it deopted {} time(s) at run \
             time; deopts by variant: {:?}",
            snapshot.deopts_by_variant.get("GcBif").copied().unwrap_or(0),
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
