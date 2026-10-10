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

//! Records instruction-family tests.
//!
//! Two halves:
//!
//! * **Fixture-driven integration** — load the committed oracle `.beam`
//!   (`tests/fixtures/records.beam`, produced by the system `erlc`) and run its
//!   exports end to end through [`Machine::run_export`], asserting the values the
//!   oracle produces. The fixture covers all three compiler reuse hints
//!   (`reuse`/`copy`/`inplace`) and the `{Updated, Original}` non-aliasing case.
//! * **Synthetic modules** — hand-built [`Module`]s (the loader is not involved)
//!   that reach the paths a compiler-generated module guards with an
//!   `is_tagged_tuple` test: a non-tuple source, a `size`/arity mismatch and an
//!   out-of-range update offset are typed [`BeamError`]s (never a panic, never a
//!   wrong result), and an update value that aliases `dst` still reads the
//!   *original* source tuple (copy-then-patch, `i_update_record_copy`).
//!
//! Every error is a typed [`BeamError`] — no test panics on runtime data.

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{
    load_file, Function, Instr, Label, Module, Operand, PutRecordArgs, UpdateRecordArgs, NO_FAIL,
};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

/// The committed records fixture (`erlc records.erl`).
const RECORDS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/records.beam");

// ---------------------------------------------------------------------------
// Fixture-driven integration
// ---------------------------------------------------------------------------

fn load(path: &str) -> Module {
    load_file(path).unwrap_or_else(|e| panic!("load {path}: {e:?}"))
}

/// `module:function/arity` for an export of `module`.
fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// A hook implementing `erlang:'+'/2` over small integers (the fixture's
/// `chain/1` computes `A + 1`).
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || function != "+" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !beam_term::is_small(args[0]) || !beam_term::is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    Ok(make_small(
        beam_term::signed_value(args[0]) + beam_term::signed_value(args[1]),
    ))
}

/// Run an export with `args` on a fresh machine over `ctx`.
fn run_export_with(
    module: &Module,
    name: &str,
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let result = machine.run_export(module, export_atom(module, name), arity, args, ctx);
    (result, machine)
}

/// Build the `#r{}` record tuple `{r, A, B, C}` of the fixture module.
fn record(ctx: &mut MinimalContext, module: &Module, a: i64, b: i64, c: i64) -> Eterm {
    let tag = make_atom(export_atom(module, "r"));
    terms::make_tuple(ctx, &[tag, make_small(a), make_small(b), make_small(c)])
        .expect("build the record tuple")
}

/// Assert `value` is the record tuple `{r, A, B, C}`.
fn assert_record(module: &Module, value: Eterm, a: i64, b: i64, c: i64) {
    assert_eq!(terms::tuple_arity(value).unwrap(), 4, "arity of {value:?}");
    assert_eq!(
        terms::tuple_element(value, 0).unwrap(),
        make_atom(export_atom(module, "r"))
    );
    assert_eq!(terms::tuple_element(value, 1).unwrap(), make_small(a));
    assert_eq!(terms::tuple_element(value, 2).unwrap(), make_small(b));
    assert_eq!(terms::tuple_element(value, 3).unwrap(), make_small(c));
}

#[test]
fn fixture_update_a_replaces_one_field() {
    // update_a(R) -> R#r{a = 10}.   (reuse hint, offset 2)
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "update_a", 1, &[arg], &mut ctx);
    let result = result.expect("update_a({r,1,2,3})");
    assert_record(&module, result, 10, 2, 3);
    // The source tuple is a distinct term and is left untouched.
    assert_record(&module, arg, 1, 2, 3);
    assert_ne!(result, arg);
}

#[test]
fn fixture_update_b_replaces_a_later_field() {
    // update_b(R) -> R#r{b = 20}.   (reuse hint, offset 3)
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "update_b", 1, &[arg], &mut ctx);
    let result = result.expect("update_b({r,1,2,3})");
    assert_record(&module, result, 1, 20, 3);
    assert_record(&module, arg, 1, 2, 3);
}

#[test]
fn fixture_update_a_c_replaces_two_fields() {
    // update_a_c(R) -> R#r{a = 11, c = 33}.   (copy hint, offsets 2 and 4)
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "update_a_c", 1, &[arg], &mut ctx);
    let result = result.expect("update_a_c({r,1,2,3})");
    assert_record(&module, result, 11, 2, 33);
    assert_record(&module, arg, 1, 2, 3);
}

#[test]
fn fixture_chain_builds_then_updates_an_inplace_hint_record() {
    // chain(A) -> R = #r{a = A}, R#r{b = A + 1}.
    // The compiler marks the update `inplace` (R is unique/dead); the engine
    // always copies, which yields the same observable tuple. The fixture's
    // oracle value is `{r,5,6,undefined}`.
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let (result, _) = run_export_with(&module, "chain", 1, &[make_small(5)], &mut ctx);
    let result = result.expect("chain(5)");
    assert_eq!(terms::tuple_arity(result).unwrap(), 4);
    assert_eq!(
        terms::tuple_element(result, 0).unwrap(),
        make_atom(export_atom(&module, "r"))
    );
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(5));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(6));
    assert_eq!(
        terms::tuple_element(result, 3).unwrap(),
        make_atom(export_atom(&module, "undefined"))
    );
}

#[test]
fn fixture_keep_returns_a_fresh_tuple_and_preserves_the_source() {
    // keep(R) -> {R#r{a = x}, R}.
    // The updated record goes to a *different* register than the source, so this
    // is the explicit non-aliasing check: the second element must still be the
    // original record. The oracle value is `{{r,x,2,3},{r,1,2,3}}`.
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "keep", 1, &[arg], &mut ctx);
    let result = result.expect("keep({r,1,2,3})");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);

    let updated = terms::tuple_element(result, 0).unwrap();
    assert_eq!(
        terms::tuple_element(updated, 0).unwrap(),
        make_atom(export_atom(&module, "r"))
    );
    assert_eq!(
        terms::tuple_element(updated, 1).unwrap(),
        make_atom(export_atom(&module, "x"))
    );
    assert_eq!(terms::tuple_element(updated, 2).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(updated, 3).unwrap(), make_small(3));

    let original = terms::tuple_element(result, 1).unwrap();
    assert_record(&module, original, 1, 2, 3);
    assert_ne!(updated, original);
}

#[test]
fn fixture_badrecord_tag_is_interned() {
    // badrecord_tag() -> badrecord.  The `badrecord` opcode carries no atom
    // operand, so the module needs this export for the interpreter to *name* the
    // `{badrecord, Value}` reason its `is_tagged_tuple` fallback raises.
    let module = load(RECORDS_BEAM);
    assert!(
        module.atom_table.lookup("badrecord").is_some(),
        "the fixture must intern the `badrecord` atom"
    );
    let mut ctx = MinimalContext::new();
    let (result, _) = run_export_with(&module, "badrecord_tag", 0, &[], &mut ctx);
    assert_eq!(
        result.expect("badrecord_tag()"),
        make_atom(export_atom(&module, "badrecord"))
    );
}

#[test]
fn fixture_non_record_raises_badrecord() {
    // non_record(V) -> V#r{a = 1}.  The compiler's `is_tagged_tuple` guard fails
    // for a non-record, so the generated `badrecord` raises `{badrecord, V}`:
    // the compiled `update_record` is never reached with a non-tuple source.
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let value = make_small(7);
    let (result, machine) = run_export_with(&module, "non_record", 1, &[value], &mut ctx);
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "expected a raise, got {result:?}"
    );
    let (class, reason) = machine.pending_exception().expect("a recorded exception");
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(terms::tuple_arity(reason).unwrap(), 2);
    assert_eq!(
        terms::tuple_element(reason, 0).unwrap(),
        make_atom(export_atom(&module, "badrecord"))
    );
    assert_eq!(terms::tuple_element(reason, 1).unwrap(), value);
}

// ---------------------------------------------------------------------------
// Synthetic modules
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for synthetic modules with an owned atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the lowest-numbered label).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Run `entry` with `args` on a fresh machine, over the **caller-owned** `ctx`.
fn run(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_label(module, entry, args.len() as u32, args, ctx)
}

/// Assert `result` is the engine's typed `unsupported` raise for `what`.
fn assert_unsupported(result: Result<Eterm, BeamError>, what: &str) {
    let want = format!("unsupported_opcode: {what}");
    match result {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some(want.as_str()),
            "unexpected unsupported reason"
        ),
        other => panic!("expected the unsupported {what:?} raise, got {other:?}"),
    }
}

/// `update_record Hint Size Src Dst Updates` as a single-instruction function.
fn update_record_fn(
    hint: Atom,
    size: u32,
    src: Operand,
    dst: Operand,
    updates: Vec<(Operand, Operand)>,
) -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)),
        Instr::UpdateRecord(Box::new(UpdateRecordArgs {
            hint: Operand::Atom(hint),
            size,
            src,
            dst,
            updates,
        })),
        Instr::Return,
    ]
}

#[test]
fn update_record_copies_the_source_and_replaces_a_field() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(3), Operand::Integer(99))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 3);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(99));
    // The source is a fresh copy, not the mutated original.
    assert_ne!(result, arg);
    assert_eq!(terms::tuple_element(arg, 2).unwrap(), make_small(3));
}

#[test]
fn update_record_applies_multiple_updates() {
    // `dst` is X(1), a different register than the source: a following `move`
    // brings the result into X(0) so `run_label` returns it.
    let mut fixture = Fixture::new();
    let copy = fixture.atom("copy");
    let entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::UpdateRecord(Box::new(UpdateRecordArgs {
                hint: Operand::Atom(copy),
                size: 4,
                src: Operand::X(0),
                dst: Operand::X(1),
                updates: vec![
                    (Operand::Integer(1), Operand::Atom(Atom::from_raw(0))),
                    (Operand::Integer(3), Operand::Integer(7)),
                ],
            })),
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(
        &mut ctx,
        &[make_small(1), make_small(2), make_small(3), make_small(4)],
    )
    .unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 4);
    assert_eq!(
        terms::tuple_element(result, 0).unwrap(),
        make_atom(Atom::from_raw(0))
    );
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(7));
    assert_eq!(terms::tuple_element(result, 3).unwrap(), make_small(4));
    // The copied source operand is untouched.
    assert_eq!(terms::tuple_element(arg, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(arg, 2).unwrap(), make_small(3));
    assert_ne!(result, arg);
}

#[test]
fn update_record_value_aliasing_dst_reads_the_original_source() {
    // `src == dst == X(0)` and the single update's value is also `X(0)`: the
    // result must embed the *original* tuple (copy-then-patch), never a
    // half-updated one.
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(3), Operand::X(0))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 3);
    let nested = terms::tuple_element(result, 2).unwrap();
    assert_eq!(terms::tuple_element(nested, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(nested, 1).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(nested, 2).unwrap(), make_small(3));
}

#[test]
fn update_record_with_a_non_tuple_source_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            2,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(2), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[make_small(5)], &mut ctx);
    assert_unsupported(result, "update_record: non-tuple source");
}

#[test]
fn update_record_with_an_out_of_range_index_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(4), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx);
    assert_unsupported(result, "update_record: index out of range");
}

#[test]
fn update_record_with_a_zero_index_is_a_typed_error() {
    // Offset 0 would name the arityval header, never a field (`Offset` is 1-based).
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(0), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx);
    assert_unsupported(result, "update_record: index out of range");
}

#[test]
fn update_record_with_a_size_mismatch_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            5,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(2), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx);
    assert_unsupported(result, "update_record: size/arity mismatch");
}

#[test]
fn update_record_ignores_an_inplace_hint_and_still_copies() {
    // An `inplace` hint (a unique/dead source) must produce an equal tuple; the
    // source term is never mutated, so a still-referenced source stays intact.
    let mut fixture = Fixture::new();
    let inplace = fixture.atom("inplace");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            inplace,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(2), Operand::Integer(42))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(42));
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(3));
    assert_eq!(terms::tuple_element(arg, 1).unwrap(), make_small(2));
    assert_ne!(result, arg);
}

// ---------------------------------------------------------------------------
// The native-record **create** form — `put_record Fail Id nil Dst Live Updates`
// ---------------------------------------------------------------------------
//
// `i_create_local_native_record` (`../../erts/emulator/beam/erl_record.c:254-353`)
// is the one native-record path **no oracle can verify**: this toolchain does not
// enable the `native_record` feature, so the compiler emits no `put_record` and
// the system `erl` can produce no ground truth. The evidence is therefore
// *structural*, built over the **real** `Recs`-chunk fixture
// (`beam_loader`'s `records_fixture.beam`), whose parsed definitions are
// themselves oracle-pinned in `beam_loader/tests/records.rs`.
//
// Each test loads that fixture, interns the reason atoms a raise needs, then
// **injects** one synthetic `f/1` function carrying `Instr::PutRecord` with
// `src: Operand::Nil` — the create form — using labels strictly above every label
// the loaded module already uses (the interpreter builds one *module-wide* label
// map, so a colliding label would be a real hazard) and runs it through
// `Machine::run_label`. Assertions read the built term back through the frozen
// `beam_term::record` / `beam_engine::terms` readers — the layout is never
// re-derived here.

/// The committed `Recs`-chunk fixture (the `beam_loader` oracle fixture), read by
/// the same relative path `beam_loader/tests/records.rs` pins.
const RECORDS_FIXTURE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/records_fixture.beam"
);

/// The first label strictly above every label the module's functions use.
fn fresh_label(module: &Module) -> u32 {
    module
        .functions
        .iter()
        .flat_map(|function| function.labels.keys())
        .map(|label| label.0)
        .max()
        .map_or(1, |max| max + 1)
}

/// Append a synthetic `name/arity` function to `module` and return its entry label
/// (the lowest label of its stream). The function has no `func_info`, because
/// `run_label` enters it directly and nothing ever jumps to it from a failed head
/// clause.
fn append_function(module: &mut Module, name: Atom, arity: u32, code: Vec<Instr>) -> Label {
    let owner = module.name;
    let labels = labels_of(&code);
    let entry = labels.keys().next().copied().unwrap_or(Label(0));
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: owner,
        name,
        arity,
        code,
        blocks: Vec::new(),
        labels,
    });
    entry
}

/// Append the synthetic create-form function `f/1`:
///
/// ```text
/// label Entry
/// put_record Fail Id nil Dst 0 Updates
/// return
/// [label Fail; move Marker X(0); return]      (only when `branch` is set)
/// ```
///
/// `fail` is [`NO_FAIL`] unless `branch` is `Some`, in which case the instruction's
/// live `fail` label is a fresh one whose block writes the atom `branch` into
/// `X(0)` and returns, so a test can observe that a *failure branched* rather than
/// raised.
fn append_create(
    module: &mut Module,
    id: Operand,
    dst: Operand,
    updates: Vec<(Operand, Operand)>,
    branch: Option<Atom>,
) -> Label {
    let base = fresh_label(module);
    let entry = Label(base);
    let fail = if branch.is_some() { Label(base + 1) } else { NO_FAIL };
    let mut code = vec![
        Instr::Label(entry),
        Instr::PutRecord(Box::new(PutRecordArgs {
            fail,
            id,
            src: Operand::Nil,
            dst,
            live: 0,
            updates,
        })),
        Instr::Return,
    ];
    if let Some(marker) = branch {
        code.push(Instr::Label(fail));
        code.push(Instr::Move {
            src: Operand::Atom(marker),
            dst: Operand::X(0),
        });
        code.push(Instr::Return);
    }
    let name = module.atom_table.intern("f");
    append_function(module, name, 1, code)
}

/// Run the injected `f/1` (arity matches `args`) and return the result plus the
/// machine, so a test can read any register the run wrote.
fn run_create(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let result = machine.run_label(module, entry, args.len() as u32, args, ctx);
    (result, machine)
}

/// The value of `record`'s field keyed `key`, or `None` when the record has no such
/// field.
#[allow(unsafe_code)]
fn record_field(module: &Module, record: Eterm, key: Atom) -> Option<Eterm> {
    // SAFETY: `record` is a live native-record term (built by the engine into the
    // context arena); the frozen reader validates the record header and only reads
    // the term's own words.
    unsafe { beam_term::record_get(record, make_atom(key), &module.atom_table) }
        .expect("the term must be a well-formed native record")
}

/// Assert `record`'s field `key` is exactly (`=:=`) `want`.
fn assert_field(module: &Module, record: Eterm, key: Atom, want: Eterm) {
    let got = record_field(module, record, key);
    assert!(
        got.is_some_and(|got| beam_term::eq(got, want, &module.atom_table)),
        "field {:?}: got {got:?}, want {want:?}",
        module.atom_name(key)
    );
}

/// Assert every field of `record` **not** named in `updated` equals the module's
/// own `Recs` default for it (an absent-sentinel field's default is `NIL`).
fn assert_defaults(module: &Module, record: Eterm, name: &str, updated: &[Atom]) {
    let definition = module
        .record_by_name(name)
        .unwrap_or_else(|| panic!("the fixture defines the record {name:?}"));
    for field in &definition.fields {
        if updated.contains(&field.name) {
            continue;
        }
        assert_field(module, record, field.name, field.default.unwrap_or(NIL));
    }
}

/// Assert `reason` is exactly `{Tag, {{Module, Name}, Field}}`
/// (`erl_record.c:339-351`).
fn assert_create_reason(module: &Module, reason: Eterm, tag: Atom, name: Atom, field: Atom) {
    assert_eq!(terms::tuple_arity(reason).unwrap(), 2, "reason arity");
    assert_eq!(terms::tuple_element(reason, 0).unwrap(), make_atom(tag), "reason tag");
    let value = terms::tuple_element(reason, 1).unwrap();
    assert_eq!(terms::tuple_arity(value).unwrap(), 2, "owned record/field pair arity");
    let owner = terms::tuple_element(value, 0).unwrap();
    assert_eq!(terms::tuple_arity(owner).unwrap(), 2, "module/name pair arity");
    assert_eq!(terms::tuple_element(owner, 0).unwrap(), make_atom(module.name));
    assert_eq!(terms::tuple_element(owner, 1).unwrap(), make_atom(name));
    assert_eq!(terms::tuple_element(value, 1).unwrap(), make_atom(field));
}

/// Assert two native records are the *same record*: the same definition identity
/// (module/name/is-exported/hash) and the same value for every field.
///
/// A structural field walk, because `beam_term::cmp`'s record class is a documented
/// raw-word fallback (two structurally-equal records in different allocations do
/// not compare equal by `=:=`).
#[allow(unsafe_code)]
fn assert_same_record(module: &Module, a: Eterm, b: Eterm, name: Atom) {
    // SAFETY: both terms are live native-record instances built by the engine.
    assert!(unsafe { beam_term::record_is_native(a, module.name, name) });
    // SAFETY: as above.
    assert!(unsafe { beam_term::record_is_native(b, module.name, name) });
    // SAFETY: as above.
    let hash_a = unsafe { beam_term::record_definition(a) }
        .and_then(|def| unsafe { beam_term::record_definition_hash(def) })
        .expect("a has a definition hash");
    // SAFETY: as above.
    let hash_b = unsafe { beam_term::record_definition(b) }
        .and_then(|def| unsafe { beam_term::record_definition_hash(def) })
        .expect("b has a definition hash");
    assert_eq!(hash_a, hash_b, "the two records must share a definition hash");

    let definition = module
        .record_by_name(name_str(module, name))
        .expect("the fixture defines the record");
    for field in &definition.fields {
        let va = record_field(module, a, field.name).expect("field on `a`");
        let vb = record_field(module, b, field.name).expect("field on `b`");
        assert!(
            beam_term::eq(va, vb, &module.atom_table),
            "field {:?}: {va:?} vs {vb:?}",
            module.atom_name(field.name)
        );
    }
}

/// The name of an interned atom (for the `record_by_name` lookup in tests).
fn name_str<'a>(module: &'a Module, name: Atom) -> &'a str {
    module.atom_name(name).expect("the atom is interned")
}

/// The `(class, reason)` of a run that must have raised.
fn raised(machine: &Machine, result: &Result<Eterm, BeamError>) -> (ExceptionClass, Eterm) {
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "expected a raise, got {result:?}"
    );
    machine.pending_exception().expect("a recorded exception")
}

/// Intern the atoms a create-form `badfield`/`novalue` raise needs to be *named*.
///
/// The fixture's atom table has none of them, so a module that does not intern them
/// can only report the typed `unsupported` (asserted below) — the same convention
/// the maps family's `{badmap,_}`/`{badkey,_}` reasons follow.
fn intern_raise_atoms(module: &mut Module) {
    for name in ["badfield", "novalue", "error"] {
        module.atom_table.intern(name);
    }
}

#[test]
fn create_applies_the_definition_defaults_and_lets_an_update_override_one() {
    // `#mixed{b = 42}`: `a` keeps its `0` default (an `i`-tagged default, distinct
    // from the absent sentinel), `c = undefined`, `d = {1,2}`, `e = [a,b]` and the
    // `10^25` bignum `f` are all defaulted; `b` (absent in the definition) is the
    // one field the update names.
    let mut module = load(RECORDS_FIXTURE_BEAM);
    let mixed = module.atom_table.intern("mixed");
    let b = module.atom_table.intern("b");
    let entry = append_create(
        &mut module,
        Operand::Atom(mixed),
        Operand::X(0),
        vec![(Operand::Atom(b), Operand::Integer(42))],
        None,
    );

    let mut ctx = MinimalContext::new();
    let (result, _) = run_create(&module, entry, &[], &mut ctx);
    let record = result.expect("creating #mixed{b = 42}");

    assert_defaults(&module, record, "mixed", &[b]);
    assert_field(&module, record, b, make_small(42));
}

#[test]
fn create_fills_unmentioned_fields_of_the_user_record_with_their_defaults() {
    // `#user{id = -1, name, city}` with only `name`/`city` set: `id` takes its `-1`
    // default. The update list is given *out of declaration order* to prove the
    // instruction sorts it (the compiled form always arrives key-sorted).
    let mut module = load(RECORDS_FIXTURE_BEAM);
    let user = module.atom_table.intern("user");
    let id = module.atom_table.intern("id");
    let name = module.atom_table.intern("name");
    let city = module.atom_table.intern("city");
    let x = module.atom_table.intern("x");
    let y = module.atom_table.intern("y");
    let entry = append_create(
        &mut module,
        Operand::Atom(user),
        Operand::X(0),
        vec![
            (Operand::Atom(city), Operand::Atom(y)),
            (Operand::Atom(name), Operand::Atom(x)),
        ],
        None,
    );

    let mut ctx = MinimalContext::new();
    let (result, _) = run_create(&module, entry, &[], &mut ctx);
    let record = result.expect("creating #user{name = x, city = y}");

    assert_defaults(&module, record, "user", &[name, city]);
    assert_field(&module, record, id, make_small(-1));
    assert_field(&module, record, name, make_atom(x));
    assert_field(&module, record, city, make_atom(y));
}

#[test]
fn create_without_a_value_raises_novalue_naming_the_first_absent_field() {
    // `#mixed{}` has no update at all, so the absent-sentinel field `b` is the
    // delayed `novalue` error. `fail == NO_FAIL` ⇒ the C's `goto find_func_info`:
    // the reason is `{novalue, {{records_fixture,mixed},b}}`, never a fall-through
    // with a wrong term.
    let mut module = load(RECORDS_FIXTURE_BEAM);
    intern_raise_atoms(&mut module);
    let mixed = module.atom_table.intern("mixed");
    let b = module.atom_table.intern("b");
    let novalue = module.atom_table.intern("novalue");
    let entry = append_create(&mut module, Operand::Atom(mixed), Operand::X(0), Vec::new(), None);

    let mut ctx = MinimalContext::new();
    let (result, machine) = run_create(&module, entry, &[], &mut ctx);
    let (class, reason) = raised(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    // Only the **first** absent field is remembered (`erl_record.c:318-326`); `b` is
    // the first in ascending key order and `city`/`name` (other absent fields of the
    // fixture) must not be named.
    assert_create_reason(&module, reason, novalue, mixed, b);
}

#[test]
fn create_badfield_outranks_the_novalue_from_the_same_run() {
    // `#mixed{zz = 1}` exercises both errors at once: `b` is absent (`novalue`) and
    // the update `zz` names no field (`badfield`). The C sets `EXC_BADFIELD` *after*
    // the walk (`erl_record.c:331-337`), so `badfield` outranks `novalue` and the
    // named field is the unconsumed update key, not `b`.
    let mut module = load(RECORDS_FIXTURE_BEAM);
    intern_raise_atoms(&mut module);
    let mixed = module.atom_table.intern("mixed");
    let zz = module.atom_table.intern("zz");
    let badfield = module.atom_table.intern("badfield");
    let entry = append_create(
        &mut module,
        Operand::Atom(mixed),
        Operand::X(0),
        vec![(Operand::Atom(zz), Operand::Integer(1))],
        None,
    );

    let mut ctx = MinimalContext::new();
    let (result, machine) = run_create(&module, entry, &[], &mut ctx);
    let (class, reason) = raised(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    assert_create_reason(&module, reason, badfield, mixed, zz);
}

#[test]
fn create_names_the_unconsumed_update_as_badfield_when_no_field_is_absent() {
    // `#user{name = x, city = y, zz = 9}`: no field is left absent (`id` has the
    // `-1` default), so the *only* error is the unconsumed `zz`.
    let mut module = load(RECORDS_FIXTURE_BEAM);
    intern_raise_atoms(&mut module);
    let user = module.atom_table.intern("user");
    let (name, city, zz, x, y) = (
        module.atom_table.intern("name"),
        module.atom_table.intern("city"),
        module.atom_table.intern("zz"),
        module.atom_table.intern("x"),
        module.atom_table.intern("y"),
    );
    let badfield = module.atom_table.intern("badfield");
    let entry = append_create(
        &mut module,
        Operand::Atom(user),
        Operand::X(0),
        vec![
            (Operand::Atom(zz), Operand::Integer(9)),
            (Operand::Atom(name), Operand::Atom(x)),
            (Operand::Atom(city), Operand::Atom(y)),
        ],
        None,
    );

    let mut ctx = MinimalContext::new();
    let (result, machine) = run_create(&module, entry, &[], &mut ctx);
    let (class, reason) = raised(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    assert_create_reason(&module, reason, badfield, user, zz);
}

#[test]
fn create_branches_to_a_live_fail_label_and_leaves_dst_untouched() {
    // With a **live** `fail` label the failure is the C's `$FAIL($Fail)` branch, not
    // a raise: control jumps to the label and the destination register is never
    // written (the record is built into `Dst` only on the success path).
    let mut module = load(RECORDS_FIXTURE_BEAM);
    let mixed = module.atom_table.intern("mixed");
    let zz = module.atom_table.intern("zz");
    let marker = module.atom_table.intern("branched");
    let entry = append_create(
        &mut module,
        Operand::Atom(mixed),
        Operand::X(1),
        vec![(Operand::Atom(zz), Operand::Integer(1))],
        Some(marker),
    );

    let mut ctx = MinimalContext::new();
    let (result, machine) = run_create(&module, entry, &[], &mut ctx);

    // The fail block wrote the marker into X(0), which `run_label` returns.
    assert_eq!(result.expect("a live fail label is a jump"), make_atom(marker));
    assert_eq!(machine.x(1), Some(NIL), "dst must be untouched on the failure branch");
    assert_eq!(machine.pending_exception(), None, "a branch is not a raise");
}

#[test]
fn create_accepts_both_the_atom_and_the_module_name_pair_id() {
    // The two id shapes the C resolves (`i_create_local_native_record` for the atom
    // form and the generic `i_create_native_record` when the module is the running
    // one) must build the *same* record. The pair rides in X(0), which the create
    // form reads (the id) before it writes it (the destination).
    let mut module = load(RECORDS_FIXTURE_BEAM);
    intern_raise_atoms(&mut module);
    let mixed = module.atom_table.intern("mixed");
    let b = module.atom_table.intern("b");

    let updates = vec![(Operand::Atom(b), Operand::Integer(7))];
    let atom_entry = append_create(
        &mut module,
        Operand::Atom(mixed),
        Operand::X(0),
        updates.clone(),
        None,
    );
    let pair_entry = append_create(&mut module, Operand::X(0), Operand::X(0), updates, None);

    let mut ctx = MinimalContext::new();
    let (from_atom, _) = run_create(&module, atom_entry, &[], &mut ctx);
    let from_atom = from_atom.expect("the atom id builds the record");

    let pair = terms::make_tuple(&mut ctx, &[make_atom(module.name), make_atom(mixed)])
        .expect("build the {Module,Name} id");
    let (from_pair, _) = run_create(&module, pair_entry, &[pair], &mut ctx);
    let from_pair = from_pair.expect("the {Module,Name} pair builds the record");

    assert_same_record(&module, from_atom, from_pair, mixed);
    // A fresh instance each time (the same inputs, a different allocation).
    assert_ne!(from_atom, from_pair);
    assert_defaults(&module, from_pair, "mixed", &[b]);
    assert_field(&module, from_pair, b, make_small(7));
}

#[test]
#[allow(unsafe_code)]
fn create_builds_a_zero_field_record() {
    // `#empty{}` has no fields: the walk is empty and the block is written with a
    // zero-field definition (`write_record`'s `n == 0` path).
    let mut module = load(RECORDS_FIXTURE_BEAM);
    let empty = module.atom_table.intern("empty");
    let entry = append_create(&mut module, Operand::Atom(empty), Operand::X(0), Vec::new(), None);

    let mut ctx = MinimalContext::new();
    let (result, _) = run_create(&module, entry, &[], &mut ctx);
    let record = result.expect("creating #empty{}");

    // SAFETY: `record` is a live native-record term built by the engine.
    assert_eq!(
        unsafe { beam_term::record_field_count(record) }.expect("a well-formed record"),
        0
    );
    // SAFETY: as above.
    assert!(unsafe { beam_term::record_values(record) }
        .expect("a well-formed record")
        .is_empty());
    // SAFETY: as above.
    assert!(
        unsafe { beam_term::record_is_native(record, module.name, empty) },
        "the built term must name the module's own `empty` definition"
    );
}

#[test]
#[allow(unsafe_code)]
fn phash2_of_a_record_depends_on_its_definition_not_the_stored_word() {
    // The record contract for `make_hash2`/`make_hash`
    // (`beam_term/src/hashing/make_hash2.rs`, the `RECORD_SUBTAG` arm, and its
    // `compute_record_definition_hash`): a record is hashed from a hash of its
    // definition's *content* —
    // `compute_record_definition_hash(def) == make_hash2({module, name, is_exported,
    // keys-in-declaration-order})` — and NOT from definition word 2 (the C reads the
    // cache there, `erl_term_hashing.c:1093-1096`; this tree's `record::write_record`
    // can only store a caller-supplied placeholder, so the value is computed instead
    // of read). Two consequences are asserted here:
    //
    // 1. the create form stores the bit-exact computed hash (never the `0` placeholder
    //    the block is first laid out with), and
    // 2. hashing does not depend on that stored word — it depends on the definition
    //    (same definition ⇒ same hash whatever the stored word; different definition
    //    ⇒ different hash).
    let mut module = load(RECORDS_FIXTURE_BEAM);
    let mixed = module.atom_table.intern("mixed");
    let other = module.atom_table.intern("mixed_other");
    let b = module.atom_table.intern("b");
    let exported = module
        .record_by_name("mixed")
        .expect("`mixed` is defined")
        .is_exported;
    let entry = append_create(
        &mut module,
        Operand::Atom(mixed),
        Operand::X(0),
        vec![(Operand::Atom(b), Operand::Integer(42))],
        None,
    );

    let mut ctx = MinimalContext::new();
    let (result, _) = run_create(&module, entry, &[], &mut ctx);
    let record = result.expect("creating #mixed{b = 42}");

    // (1) The stored definition word is the bit-exact computed hash.
    // SAFETY: `record` is a live native-record term built by the engine.
    let definition = unsafe { beam_term::record_definition(record) }.expect("a definition");
    // SAFETY: `definition` is the record's own (live) definition block.
    let stored = unsafe { beam_term::record_definition_hash(definition) }.expect("the hash word");
    assert!(
        beam_term::is_small(stored),
        "the cached hash is a small int, got {stored:?}"
    );
    let stored = beam_term::signed_value(stored);
    assert_ne!(stored, 0, "the `0` placeholder must not survive the create form");
    let computed =
        beam_term::hashing::compute_record_definition_hash(definition, &module.atom_table)
            .expect("the definition hash");
    assert_eq!(stored, i64::from(computed));

    // The created record's field values in declaration order, so an independently
    // written block can reproduce the *same* definition content.
    let fields = module
        .record_by_name("mixed")
        .expect("`mixed` is defined")
        .fields
        .clone();
    let pairs: Vec<(Eterm, Eterm)> = fields
        .iter()
        .map(|field| {
            (
                make_atom(field.name),
                record_field(&module, record, field.name).expect("every field is set"),
            )
        })
        .collect();

    // (2a) The SAME definition written with a `0` placeholder hash: the two records
    // hash identically because the stored word is not an input.
    let hp = ctx
        .alloc_words(beam_term::record_block_words(pairs.len()))
        .expect("arena words");
    // SAFETY: `hp` owns `record_block_words(n)` exclusively-owned words, `pairs` are
    // declaration-order atom-keyed pairs, and every term stays live in `ctx`.
    let placeholder =
        unsafe { beam_term::write_record(hp, module.name, mixed, exported, &pairs, make_small(0)) }
            .expect("the placeholder block");
    // SAFETY: `placeholder` is a live native-record term (its own definition block).
    let placeholder_def =
        unsafe { beam_term::record_definition(placeholder) }.expect("a definition");
    // SAFETY: `placeholder_def` is the placeholder record's own definition block.
    let placeholder_word =
        unsafe { beam_term::record_definition_hash(placeholder_def) }.expect("the hash word");
    assert_eq!(
        beam_term::signed_value(placeholder_word),
        0,
        "the two records must really carry different stored hash words"
    );
    assert_eq!(
        beam_term::hashing::make_hash2(record, &module.atom_table).expect("phash2 of the record"),
        beam_term::hashing::make_hash2(placeholder, &module.atom_table)
            .expect("phash2 of the placeholder"),
        "the stored definition-hash word must not change `phash2/1` of the record"
    );
    assert_eq!(
        beam_term::hashing::make_hash(record, &module.atom_table).expect("phash of the record"),
        beam_term::hashing::make_hash(placeholder, &module.atom_table)
            .expect("phash of the placeholder"),
        "the stored definition-hash word must not change `phash/1` of the record"
    );

    // (2b) A DIFFERENT definition (a different record `name`, the same field values)
    // must hash differently — the hash genuinely follows the definition, so (2a) is
    // not vacuous. Its stored word is a *stale non-zero* value from the other
    // definition, which hashing must ignore as well.
    let hp_other = ctx
        .alloc_words(beam_term::record_block_words(pairs.len()))
        .expect("arena words");
    // SAFETY: `hp_other` owns `record_block_words(n)` exclusively-owned words and
    // `pairs` stays live in `ctx`.
    let other_record = unsafe {
        beam_term::write_record(hp_other, module.name, other, exported, &pairs, make_small(stored))
    }
    .expect("the other-definition block");
    assert_ne!(
        beam_term::hashing::make_hash2(record, &module.atom_table).expect("phash2 of the record"),
        beam_term::hashing::make_hash2(other_record, &module.atom_table)
            .expect("phash2 of the other definition"),
        "a record with a different definition must hash differently"
    );
    assert_ne!(
        beam_term::hashing::make_hash(record, &module.atom_table).expect("phash of the record"),
        beam_term::hashing::make_hash(other_record, &module.atom_table)
            .expect("phash of the other definition"),
        "a record with a different definition must hash differently"
    );
}

#[test]
fn create_without_the_reason_atom_is_a_typed_unsupported() {
    // The fixture interns none of `badfield`/`novalue`/`error`, so a create failure
    // cannot *name* its reason. The engine reports the typed capability gap rather
    // than substituting a different atom (never a wrong reason).
    let mut module = load(RECORDS_FIXTURE_BEAM);
    let mixed = module.atom_table.intern("mixed");
    let entry = append_create(&mut module, Operand::Atom(mixed), Operand::X(0), Vec::new(), None);

    let mut ctx = MinimalContext::new();
    let (result, _) = run_create(&module, entry, &[], &mut ctx);
    assert_unsupported(
        result,
        "put_record: create: no 'novalue' atom in the module",
    );
}
