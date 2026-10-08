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

//! Baseline-JIT **comparison fast-path** integration test.
//!
//! `src/jit/baseline/emit/compare.rs` lowers the eight `erlang` comparison BIFs
//! (`=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`) into native x86-64 for in-bound
//! x-registers and immediate (`small`/atom/`nil`) constants, materialising the boolean
//! as the **module's own** `true`/`false` atom word **without** ever calling the BIF
//! (`ExecutionContext::call_bif`). `src/jit/baseline/emit/test.rs` lowers
//! `is_eq_exact`/`is_ne_exact` over two small-int x-registers the same way. Neither
//! path had dedicated coverage.
//!
//! This file pins three things against [`Tier::Interpreter`] as the oracle (both tiers
//! are reached through `Machine::run_with_tier`, the runtime's own tier selector, and
//! — in one test — through the [`JitBackend`] compile-once/run-many entry):
//!
//! 1. **Interpreter-identity** of every inlined shape — two small-int registers, a
//!    small-int constant on either side, the guard-free atom/`nil` constant forms, the
//!    `gc_bif` spelling, a destination that is not `x(0)`, and the both-register
//!    `is_eq_exact`/`is_ne_exact` `test` — over a sweep of small-int boundaries
//!    ([`MIN_SMALL`]/[`MAX_SMALL`] included, where the tagged-word signed order is
//!    easiest to get wrong).
//! 2. **Guard-rejection / refusal deopts** to the interpreter's *exact* outcome —
//!    a boxed float where a raw word compare would be wrong (`5.0 =:= 5` is `false`
//!    yet `5.0 == 5` is `true`), a bignum constant, an atom/`nil` under an ordering
//!    operator, and a `Y`/float-register/literal operand — including on the typed
//!    error, never a fabricated value.
//! 3. **The inlining evidence.** The baseline tier has no `CodegenReport`, so the
//!    primary evidence is behavioural: a counting BIF floor shows the inlined shape
//!    invokes the hook **zero** times while a guard rejection or a refused shape
//!    invokes it. A secondary artifact-level signal is
//!    [`CompiledModule::highest_native_register`] — set for a comparison-only block,
//!    `None` for a block that must stay on the helper.
//!
//! The fixtures are hand-built `beam_loader::Module`s (the loader is not involved) so
//! each shape is exercised in isolation; the patterns mirror `tests/jit_cranelift.rs`'s
//! comparison coverage, retargeted at [`Tier::BaselineJit`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_common::BeamError;
use beam_engine::jit::baseline::{BaselineJit, CompiledModule};
use beam_engine::jit::JitBackend;
use beam_engine::{terms, Machine, MinimalContext, Tier, MAX_REGISTER};
use beam_loader::{
    Export, Function, Import, ImportRef, Instr, Label, Module, Operand, TestKind, NO_FAIL,
};
use beam_term::{make_atom, make_small, Atom, Eterm, MAX_SMALL, MIN_SMALL, NIL};

// ---------------------------------------------------------------------------
// Shared fixture plumbing
// ---------------------------------------------------------------------------

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// Run `export/arity` of `module` on `tier` with `ctx` as the execution context.
fn run_module(
    tier: Tier,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_with_tier(module, atom(module, export), arity, args, ctx, tier)
}

/// Run `export/arity` of `module` on `tier` with `hook` as the BIF floor.
fn run_hooked<H>(
    tier: Tier,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: H,
) -> Result<Eterm, BeamError>
where
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(hook));
    machine.run_with_tier(module, atom(module, export), arity, args, &mut ctx, tier)
}

/// Run `export/arity` of `module` through the **baseline tier on a caller-owned
/// compiled artifact** — the compile-once / run-many
/// [`JitBackend::run_compiled`] entry — with `hook` as the BIF floor.
fn run_compiled_hooked<H>(
    backend: &BaselineJit,
    compiled: &CompiledModule,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: H,
) -> Result<Eterm, BeamError>
where
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(hook));
    backend.run_compiled(
        compiled,
        &mut machine,
        module,
        atom(module, export),
        arity,
        args,
        &mut ctx,
    )
}

/// Assert two outcomes agree: the same value, or the same typed error.
fn assert_same(got: &Result<Eterm, BeamError>, interpreted: &Result<Eterm, BeamError>, what: &str) {
    match (got, interpreted) {
        (Ok(got), Ok(interpreted)) => {
            assert_eq!(got, interpreted, "{what} differs from the interpreter")
        }
        (Err(got), Err(interpreted)) => assert_eq!(
            format!("{got:?}"),
            format!("{interpreted:?}"),
            "{what} disagrees with the interpreter on the typed error"
        ),
        _ => panic!("{what}: got {got:?}, the interpreter produced {interpreted:?}"),
    }
}

/// The comparison-BIF counterpart of `tests/jit_cranelift.rs`'s `agree_comparison`:
/// assert the interpreter and the **baseline tier** produce the same outcome for
/// `export/arity` under the same comparison floor, and return that outcome.
/// `make_hook` builds one fresh floor per tier (each tier *owns* its hook).
fn agree_comparison<M, H>(
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    mut make_hook: M,
) -> Result<Eterm, BeamError>
where
    M: FnMut() -> H,
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let interpreted = run_hooked(Tier::Interpreter, module, export, arity, args, make_hook());
    let baseline = run_hooked(Tier::BaselineJit, module, export, arity, args, make_hook());
    assert_same(&baseline, &interpreted, &format!("baseline {export}/{arity}"));
    interpreted
}

/// Assert the interpreter and the baseline tier agree for `export/arity` with an empty
/// [`MinimalContext`] (no BIF floor) — for the shapes whose *only* correct outcome is a
/// typed error that both tiers must surface identically.
fn agree_module(module: &Module, export: &str, arity: u32, args: &[Eterm]) -> Result<Eterm, BeamError> {
    let interpreted = run_module(
        Tier::Interpreter,
        module,
        export,
        arity,
        args,
        &mut MinimalContext::new(),
    );
    let baseline = run_module(
        Tier::BaselineJit,
        module,
        export,
        arity,
        args,
        &mut MinimalContext::new(),
    );
    assert_same(&baseline, &interpreted, &format!("baseline {export}/{arity}"));
    interpreted
}

// ---------------------------------------------------------------------------
// The eight comparison operators and their ground truth
// ---------------------------------------------------------------------------

/// The eight comparison operators with the three fixture names each one uses: the
/// two-x-register form, the `X(0) <op> 5` form, and the `5 <op> X(0)` form.
const COMPARISONS: [(&str, &str, &str, &str); 8] = [
    ("=:=", "exact_eq", "exact_eq_const", "exact_eq_const_lhs"),
    ("=/=", "exact_ne", "exact_ne_const", "exact_ne_const_lhs"),
    ("==", "arith_eq", "arith_eq_const", "arith_eq_const_lhs"),
    ("/=", "arith_ne", "arith_ne_const", "arith_ne_const_lhs"),
    ("<", "lt", "lt_const", "lt_const_lhs"),
    ("=<", "le", "le_const", "le_const_lhs"),
    (">", "gt", "gt_const", "gt_const_lhs"),
    (">=", "ge", "ge_const", "ge_const_lhs"),
];

/// The oracle answer of `a <op> b` for two small integers (where `=:=` and `==`, and
/// `=/=` and `/=`, coincide).
fn comparison_oracle(op: &str, a: i64, b: i64) -> bool {
    match op {
        "=:=" | "==" => a == b,
        "=/=" | "/=" => a != b,
        "<" => a < b,
        "=<" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        other => panic!("unknown comparison operator {other:?}"),
    }
}

/// The fixture module's own `true`/`false` atom as a term — a comparison BIF answers
/// with an **atom**, not a small integer.
fn boolean(module: &Module, answer: bool) -> Eterm {
    make_atom(atom(module, if answer { "true" } else { "false" }))
}

/// A name resolved in a cloned atom table (see [`comparison_floor`]).
fn atom_of(atoms: &beam_term::AtomTable, name: &str) -> Atom {
    atoms
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// The ground-truth floor of the eight comparison BIFs: `beam_term`'s own exact /
/// arithmetic ordering against a clone of the fixture module's atom table. The
/// interpreter's answer therefore comes from the *real* ordering, so the tier's word
/// compare is checked against it rather than against a second implementation of it.
fn comparison_floor(
    atoms: beam_term::AtomTable,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    move |module, function, arity, args| {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let (a, b) = (args[0], args[1]);
        let answer = match function {
            "=:=" => beam_term::cmp(a, b, &atoms).is_eq(),
            "=/=" => !beam_term::cmp(a, b, &atoms).is_eq(),
            "==" => beam_term::cmp_arith(a, b, &atoms).is_eq(),
            "/=" => !beam_term::cmp_arith(a, b, &atoms).is_eq(),
            "<" => beam_term::cmp_arith(a, b, &atoms).is_lt(),
            "=<" => !beam_term::cmp_arith(a, b, &atoms).is_gt(),
            ">" => beam_term::cmp_arith(a, b, &atoms).is_gt(),
            ">=" => !beam_term::cmp_arith(a, b, &atoms).is_lt(),
            _ => return Err(BeamError::BadArg),
        };
        let name = if answer { "true" } else { "false" };
        Ok(make_atom(atom_of(&atoms, name)))
    }
}

/// [`comparison_floor`] behind a call counter: the same behaviour, with every call
/// counted, so a test can tell "the fast path computed this" (zero calls) from "the
/// guard rejected this and the helper ran the BIF" (one call).
fn counting_comparison_floor(
    atoms: beam_term::AtomTable,
    calls: Arc<AtomicUsize>,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    let mut floor = comparison_floor(atoms);
    move |module, function, arity, args| {
        calls.fetch_add(1, Ordering::SeqCst);
        floor(module, function, arity, args)
    }
}

// ---------------------------------------------------------------------------
// The comparison-BIF fixture builder
// ---------------------------------------------------------------------------

/// One operand of a hand-built comparison fixture.
#[derive(Clone, Copy)]
enum ArithSpec {
    /// The x-register `n` — a register operand the fast path can inline.
    X(u32),
    /// An integer constant operand (`{integer,V}`): an inlinable **small-int**
    /// constant when `V` is inside the 60-bit range, otherwise the bignum constant
    /// the fast path refuses.
    Int(i64),
    /// A float-register operand (`z,2`) — never a small-int constant, so the fast
    /// path refuses it.
    FloatReg(u32),
    /// A static-literal operand (`z,4`) — never a small-int constant either.
    Literal(u32),
    /// A `Y` (stack slot) operand — neither an x-register nor a constant.
    Y(u32),
    /// An atom constant operand, interned in the fixture module's atom table.
    Atom(&'static str),
    /// The `nil` constant operand.
    Nil,
}

impl ArithSpec {
    /// The loader operand this spec stands for, interning an atom name in `atoms`.
    fn operand(self, atoms: &mut beam_term::AtomTable) -> Operand {
        match self {
            ArithSpec::X(n) => Operand::X(n),
            ArithSpec::Int(value) => Operand::Integer(value),
            ArithSpec::FloatReg(n) => Operand::FloatReg(n),
            ArithSpec::Literal(index) => Operand::Literal(index),
            ArithSpec::Y(n) => Operand::Y(n),
            ArithSpec::Atom(name) => Operand::Atom(atoms.intern(name)),
            ArithSpec::Nil => Operand::Nil,
        }
    }
}

/// One hand-built exported function of a comparison fixture:
///
/// ```text
///   label E:  {bif|gc_bif} M:F/2 [<args>] -> <dst>
///             [move <dst> -> x(0)]        ; only when `tail_move`
///             return
/// ```
struct CmpFn {
    /// The exported name.
    name: &'static str,
    /// The arity of the *exported* function (the number of x-register arguments it
    /// reads) — never the BIF's own arity, which is always 2 here.
    arity: u32,
    /// The imported module name (`"erlang"` for every fast-path-eligible shape).
    module: &'static str,
    /// The imported function name (one of [`COMPARISONS`] for the positive shapes).
    bif: &'static str,
    /// The argument operands, in BIF argument order.
    args: Vec<ArithSpec>,
    /// The destination operand.
    dst: ArithSpec,
    /// Whether the call is spelled `gc_bif` (the compiler emits `bif` for comparisons,
    /// but the tier inlines both variants).
    gc: bool,
    /// An instruction arity that differs from the import's (a hand-built shape the tier
    /// must refuse), when set.
    instr_arity: Option<u8>,
    /// Whether the fixture appends `move <dst> -> x(0)`, so a fixture whose destination
    /// is **not** `x(0)` still answers with its result.
    tail_move: bool,
}

impl CmpFn {
    /// A `bif` fixture named `name`, exported at `arity`, calling `erlang:<bif>/2` over
    /// `args` into `dst`.
    fn new(
        name: &'static str,
        arity: u32,
        bif: &'static str,
        args: &[ArithSpec],
        dst: ArithSpec,
    ) -> CmpFn {
        CmpFn {
            name,
            arity,
            module: "erlang",
            bif,
            args: args.to_vec(),
            dst,
            gc: false,
            instr_arity: None,
            tail_move: false,
        }
    }

    /// Spell the call `gc_bif` instead of `bif`.
    fn gc(mut self) -> CmpFn {
        self.gc = true;
        self
    }

    /// Call `<module>:<bif>/2` instead of `erlang:<bif>/2`.
    fn in_module(mut self, module: &'static str) -> CmpFn {
        self.module = module;
        self
    }

    /// Emit an instruction arity that differs from the import's.
    fn with_instr_arity(mut self, arity: u8) -> CmpFn {
        self.instr_arity = Some(arity);
        self
    }

    /// Append `move <dst> -> x(0)` before the `return`.
    fn moving_the_result_to_x0(mut self) -> CmpFn {
        self.tail_move = true;
        self
    }
}

/// Build a hand-built module (the loader is not involved) whose exported functions are
/// exactly `fixtures`, against a fresh import per function — with `true`/`false`
/// interned.
fn comparison_bif_module(name: &str, fixtures: &[CmpFn]) -> Module {
    comparison_bif_module_with_atoms(name, fixtures, true)
}

/// A comparison fixture whose atom table does **not** carry `true`/`false`: the result
/// words cannot be resolved, so the whole instruction must stay on the helper.
fn comparison_bif_module_without_booleans(name: &str, fixtures: &[CmpFn]) -> Module {
    comparison_bif_module_with_atoms(name, fixtures, false)
}

/// The shared builder of [`comparison_bif_module`].
fn comparison_bif_module_with_atoms(name: &str, fixtures: &[CmpFn], booleans: bool) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    if booleans {
        module.atom_table.intern("true");
        module.atom_table.intern("false");
    }

    let mut next_label = 1u32;
    for fixture in fixtures {
        let export = module.atom_table.intern(fixture.name);
        let module_atom = module.atom_table.intern(fixture.module);
        let function_atom = module.atom_table.intern(fixture.bif);
        module.imports.push(Import {
            module: module_atom,
            function: function_atom,
            arity: fixture.args.len() as u32,
        });
        let bif = ImportRef((module.imports.len() - 1) as u32);

        let label = Label(next_label);
        next_label += 1;
        let arity = fixture.instr_arity.unwrap_or(fixture.args.len() as u8);
        let args: Vec<Operand> = fixture
            .args
            .iter()
            .map(|spec| spec.operand(&mut module.atom_table))
            .collect();
        let dst = fixture.dst.operand(&mut module.atom_table);
        let call = if fixture.gc {
            Instr::GcBif {
                arity,
                bif,
                fail: NO_FAIL,
                live: fixture.arity,
                args,
                dst,
            }
        } else {
            Instr::Bif {
                arity,
                bif,
                fail: NO_FAIL,
                args,
                dst,
            }
        };
        let mut code = vec![Instr::Label(label), call];
        if fixture.tail_move {
            code.push(Instr::Move {
                src: fixture.dst.operand(&mut module.atom_table),
                dst: Operand::X(0),
            });
        }
        code.push(Instr::Return);

        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: label,
            entry_label: label,
            module: module.name,
            name: export,
            arity: fixture.arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity: fixture.arity,
            label,
        });
    }
    module
}

/// The fast-path comparison fixtures: the eight operators over two x-registers, the
/// eight with a small-int constant on either side (so both the `Guard::Lhs` and the
/// `Guard::Rhs` shapes are covered), a `gc_bif` spelling, a destination that is not
/// `x(0)`, and the two `Guard::None` constant forms (an atom under `==`, `nil` under
/// `=:=`).
fn comparison_bif_fixtures() -> Module {
    let mut fixtures: Vec<CmpFn> = Vec::new();
    for &(op, regs, const_rhs, const_lhs) in &COMPARISONS {
        fixtures.push(CmpFn::new(
            regs,
            2,
            op,
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(0),
        ));
        fixtures.push(CmpFn::new(
            const_rhs,
            1,
            op,
            &[ArithSpec::X(0), ArithSpec::Int(5)],
            ArithSpec::X(0),
        ));
        fixtures.push(CmpFn::new(
            const_lhs,
            1,
            op,
            &[ArithSpec::Int(5), ArithSpec::X(0)],
            ArithSpec::X(0),
        ));
    }
    fixtures.push(
        CmpFn::new(
            "gc_eq",
            2,
            "=:=",
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(0),
        )
        .gc(),
    );
    fixtures.push(
        CmpFn::new(
            "gt_dst_two",
            2,
            ">",
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(2),
        )
        .moving_the_result_to_x0(),
    );
    fixtures.push(CmpFn::new(
        "arith_eq_atom",
        1,
        "==",
        &[ArithSpec::X(0), ArithSpec::Atom("a")],
        ArithSpec::X(0),
    ));
    fixtures.push(CmpFn::new(
        "exact_eq_nil",
        1,
        "=:=",
        &[ArithSpec::X(0), ArithSpec::Nil],
        ArithSpec::X(0),
    ));
    comparison_bif_module("cmpbif", &fixtures)
}

/// The shapes the tier must **not** inline — one per clause of the fast path's shape
/// test: another BIF, the right BIF name in the wrong module, a non-2 arity (the
/// import's and the instruction's), a `Y`/float-register/literal operand, a bignum
/// constant, an atom and a `nil` constant under an *ordering* operator, a `Y`
/// destination, and a destination past [`MAX_REGISTER`].
fn non_inlinable_comparison_module() -> Module {
    comparison_bif_module(
        "cmpbif_helper",
        &[
            CmpFn::new(
                "divide",
                2,
                "div",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "eq_foreign",
                2,
                "==",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            )
            .in_module("mymod"),
            CmpFn::new(
                "eq_arity3",
                1,
                "==",
                &[ArithSpec::X(0), ArithSpec::Int(1), ArithSpec::Int(2)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "eq_instr_arity3",
                2,
                "==",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            )
            .with_instr_arity(3),
            CmpFn::new(
                "lt_y_operand",
                2,
                "<",
                &[ArithSpec::X(0), ArithSpec::Y(0)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_floatreg",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::FloatReg(0)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_literal",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Literal(0)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_bigconst",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Int(i64::MAX)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_atom",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Atom("a")],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_nil",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Nil],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_y_dst",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Int(5)],
                ArithSpec::Y(0),
            ),
            CmpFn::new(
                "lt_dst_oob",
                2,
                "<",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(MAX_REGISTER),
            ),
        ],
    )
}

// ---------------------------------------------------------------------------
// The `test`-instruction fixture builder (for `is_eq_exact`/`is_ne_exact`)
// ---------------------------------------------------------------------------

/// One operand of a hand-built `test`.
#[derive(Clone, Copy)]
enum Spec {
    /// The x-register `n`.
    X(u32),
    /// A small-integer constant operand (`{integer,V}`).
    Int(i64),
    /// The `nil` constant operand.
    Nil,
    /// An atom constant operand, interned in the fixture module's atom table.
    Atom(&'static str),
}

/// One hand-built exported function: `test <kind> [<operands>]`, answering `1` when
/// the test succeeds and `0` when it branches to its `fail` label.
struct TestFn {
    /// The exported name.
    name: &'static str,
    /// The arity (the number of x-register arguments the test reads).
    arity: u32,
    /// The classified test.
    kind: TestKind,
    /// The tested operands, in file order.
    operands: Vec<Spec>,
}

impl TestFn {
    /// A fixture named `name`, of `arity` arguments, running `kind` over `operands`.
    fn new(name: &'static str, arity: u32, kind: TestKind, operands: &[Spec]) -> TestFn {
        TestFn {
            name,
            arity,
            kind,
            operands: operands.to_vec(),
        }
    }
}

/// Build a hand-built module (the loader is not involved) whose exported functions
/// are exactly `functions`, each one basic-block pair:
///
/// ```text
///   label E:  test <kind> [<operands>] F
///             move {integer,1} -> x0    ; the test succeeded
///             return
///   label F:  move {integer,0} -> x0    ; the test failed
///             return
/// ```
fn test_module(name: &str, functions: &[TestFn]) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    module.atom_table.intern("true");
    module.atom_table.intern("false");

    let mut next_label = 1u32;
    for fixture in functions {
        let export = module.atom_table.intern(fixture.name);
        let entry = Label(next_label);
        next_label += 1;
        let fail = Label(next_label);
        next_label += 1;
        let args: Vec<Operand> = fixture
            .operands
            .iter()
            .map(|spec| match spec {
                Spec::X(n) => Operand::X(*n),
                Spec::Int(value) => Operand::Integer(*value),
                Spec::Nil => Operand::Nil,
                Spec::Atom(atom) => Operand::Atom(module.atom_table.intern(atom)),
            })
            .collect();
        let code = vec![
            Instr::Label(entry),
            Instr::Test {
                kind: fixture.kind,
                fail,
                args,
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(fail),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module: module.name,
            name: export,
            arity: fixture.arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity: fixture.arity,
            label: entry,
        });
    }
    module
}

/// The `1`/`0` answer a `test` fixture answers with.
fn flag(condition: bool) -> Eterm {
    make_small(if condition { 1 } else { 0 })
}

/// The `is_eq_exact`/`is_ne_exact` fixtures — the two-x-register shapes
/// `src/jit/baseline/emit/test.rs` newly inlines by guarding **both** registers as
/// small ints, plus the guard-free constant forms (a small, an atom and `nil`).
fn exact_test_module() -> Module {
    test_module(
        "cmp_exact",
        &[
            TestFn::new("eq_exact_regs", 2, TestKind::IsEqExact, &[Spec::X(0), Spec::X(1)]),
            TestFn::new("ne_exact_regs", 2, TestKind::IsNeExact, &[Spec::X(0), Spec::X(1)]),
            TestFn::new(
                "eq_exact_const",
                1,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::Int(5)],
            ),
            TestFn::new(
                "eq_exact_atom",
                1,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::Atom("a")],
            ),
            TestFn::new("eq_exact_nil", 1, TestKind::IsEqExact, &[Spec::X(0), Spec::Nil]),
        ],
    )
}

// ---------------------------------------------------------------------------
// 1. Interpreter-identity of the inlined comparison shapes
// ---------------------------------------------------------------------------

#[test]
fn the_inlined_comparison_bifs_agree_over_a_wide_range() {
    // Every inlined shape is pinned on both its pass and its fail side, across the whole
    // small-int range (boundaries included, where the tagged-word order is easiest to
    // get wrong), against the interpreter **and** an independently computed oracle.
    let module = comparison_bif_fixtures();
    let atoms = module.atom_table.clone();

    let values = [-40i64, -1, 0, 1, 5, 6, 40, MIN_SMALL, MAX_SMALL];
    for &(op, regs, const_rhs, const_lhs) in &COMPARISONS {
        for &a in &values {
            for &b in &values {
                let args = [make_small(a), make_small(b)];
                let got = agree_comparison(&module, regs, 2, &args, || comparison_floor(atoms.clone()));
                assert_eq!(
                    got.ok(),
                    Some(boolean(&module, comparison_oracle(op, a, b))),
                    "{op}: {a} vs {b}"
                );
            }
            let arg = [make_small(a)];
            let got = agree_comparison(&module, const_rhs, 1, &arg, || {
                comparison_floor(atoms.clone())
            });
            assert_eq!(
                got.ok(),
                Some(boolean(&module, comparison_oracle(op, a, 5))),
                "{op}: {a} vs 5"
            );
            let got = agree_comparison(&module, const_lhs, 1, &arg, || {
                comparison_floor(atoms.clone())
            });
            assert_eq!(
                got.ok(),
                Some(boolean(&module, comparison_oracle(op, 5, a))),
                "{op}: 5 vs {a}"
            );
        }
    }

    // The `gc_bif` spelling and the non-`x(0)` destination.
    let args = [make_small(7), make_small(7)];
    for (export, expected) in [("gc_eq", true), ("gt_dst_two", false)] {
        let got = agree_comparison(&module, export, 2, &args, || comparison_floor(atoms.clone()));
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "{export}");
    }

    // The two guard-free constant forms: an atom under `==` (identical word only) and
    // `nil` under `=:=`.
    let an_atom = make_atom(atom(&module, "a"));
    for (arg, expected) in [(an_atom, true), (make_small(7), false), (NIL, false)] {
        let got = agree_comparison(&module, "arith_eq_atom", 1, &[arg], || {
            comparison_floor(atoms.clone())
        });
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "arith_eq_atom");
    }
    for (arg, expected) in [(NIL, true), (make_small(7), false), (an_atom, false)] {
        let got = agree_comparison(&module, "exact_eq_nil", 1, &[arg], || {
            comparison_floor(atoms.clone())
        });
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "exact_eq_nil");
    }
}

#[test]
fn the_inlined_exact_test_agrees_on_two_small_registers() {
    // `is_eq_exact`/`is_ne_exact` over two x-registers are inlined by guarding **both**
    // as small ints (`src/jit/baseline/emit/test.rs`). Sweep the same boundaries; the
    // oracle is the exact order over two integers.
    let module = exact_test_module();

    let values = [-40i64, -1, 0, 1, 5, 6, 40, MIN_SMALL, MAX_SMALL];
    for &a in &values {
        for &b in &values {
            let args = [make_small(a), make_small(b)];
            let got = agree_module(&module, "eq_exact_regs", 2, &args);
            assert_eq!(got.ok(), Some(flag(a == b)), "=:=: {a} vs {b}");
            let got = agree_module(&module, "ne_exact_regs", 2, &args);
            assert_eq!(got.ok(), Some(flag(a != b)), "=/=: {a} vs {b}");
        }
    }

    // The small-constant form (`Guard::None`) and the constant-`immediate` forms.
    for &a in &values {
        let got = agree_module(&module, "eq_exact_const", 1, &[make_small(a)]);
        assert_eq!(got.ok(), Some(flag(a == 5)), "=:= {a}, 5");
    }
    let an_atom = make_atom(atom(&module, "a"));
    for (arg, expected) in [(an_atom, true), (make_small(7), false), (NIL, false)] {
        let got = agree_module(&module, "eq_exact_atom", 1, &[arg]);
        assert_eq!(got.ok(), Some(flag(expected)), "=:= arg, a");
    }
    for (arg, expected) in [(NIL, true), (make_small(7), false), (an_atom, false)] {
        let got = agree_module(&module, "eq_exact_nil", 1, &[arg]);
        assert_eq!(got.ok(), Some(flag(expected)), "=:= arg, nil");
    }

    // A non-small operand makes the run-time guard reject, so the whole `test` deopts
    // to the helper — whose exact `cmp` answer must be reproduced (a raw word compare
    // would wrongly equate or distinguish the boxed float).
    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    for (a, b, expected) in [
        (five_float, make_small(5), false),
        (make_small(5), five_float, false),
        (five_float, five_float, true),
        (NIL, make_small(5), false),
    ] {
        let got = agree_module(&module, "eq_exact_regs", 2, &[a, b]);
        assert_eq!(got.ok(), Some(flag(expected)), "=:= guard rejection");
        let got = agree_module(&module, "ne_exact_regs", 2, &[a, b]);
        assert_eq!(got.ok(), Some(flag(!expected)), "=/= guard rejection");
    }
}

// ---------------------------------------------------------------------------
// 2. Guard-rejection / refusal deopts
// ---------------------------------------------------------------------------

#[test]
fn a_non_small_comparison_operand_deopts_to_the_helper_and_still_agrees() {
    // The operand guard is a *run-time* test, so the very same instruction inlines one
    // moment and deopts the next. These cases force the deopt: a boxed float cannot be a
    // tagged small word, and neither can a bignum, an atom or `[]`.
    let module = comparison_bif_fixtures();
    let atoms = module.atom_table.clone();

    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    let four_float = terms::make_float(&mut arena, 4.0).expect("the arena allocates a float");
    let bignum = terms::make_integer(&mut arena, i64::MAX).expect("the arena allocates a bignum");
    let an_atom = make_atom(atom(&module, "a"));

    // The float cases are the interesting ones: `5.0 =:= 5` is `false` (the exact
    // compare is class-strict) while `5.0 == 5` is `true` (the arithmetic compare is
    // value-based) — so a raw tagged-word compare on the boxed pointer would answer
    // `false`/`false` and be caught here.
    for (export, args, expected) in [
        ("exact_eq", [five_float, make_small(5)], false),
        ("exact_ne", [five_float, make_small(5)], true),
        ("arith_eq", [five_float, make_small(5)], true),
        ("arith_ne", [five_float, make_small(5)], false),
        ("lt", [four_float, make_small(5)], true),
        ("gt", [four_float, make_small(5)], false),
        ("le", [five_float, make_small(5)], true),
        ("ge", [five_float, make_small(5)], true),
    ] {
        let got = agree_comparison(&module, export, 2, &args, || comparison_floor(atoms.clone()));
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "{export}(float)");
    }

    // A bignum, an atom and `[]` against a small: agreed with the interpreter (the
    // comparison itself has no oracle worth hand-writing, but the deopt must not
    // fabricate a word-compare answer).
    for bad in [bignum, an_atom, NIL] {
        for &(op, regs, ..) in &COMPARISONS {
            let args = [bad, make_small(5)];
            let got = agree_comparison(&module, regs, 2, &args, || comparison_floor(atoms.clone()));
            assert!(got.is_ok(), "{op}(boxed, 5) must answer, not raise: {got:?}");
        }
    }
}

#[test]
fn non_inlinable_comparison_shapes_stay_on_the_helper() {
    // Each fixture violates exactly one clause of the fast path's shape test, so the
    // shapes must still run with the helper's *exact* semantics (the interpreter's
    // answer on every tier).
    let module = non_inlinable_comparison_module();
    let atoms = module.atom_table.clone();

    // Another BIF, the right BIF name in the wrong module, and a non-2 BIF arity: the
    // floor receives a call it rejects, so every tier reports its typed `BadArg`.
    for (export, arity, args) in [
        ("divide", 2, vec![make_small(6), make_small(7)]),
        ("eq_foreign", 2, vec![make_small(6), make_small(7)]),
        ("eq_arity3", 1, vec![make_small(6)]),
    ] {
        let got = agree_comparison(&module, export, arity, &args, || comparison_floor(atoms.clone()));
        assert!(matches!(got, Err(BeamError::BadArg)), "{export}: got {got:?}");
    }

    // A *non-2 instruction* arity over a 2-arity import: the interpreter resolves the
    // import (so it computes the comparison), while the tier's shape test refuses the
    // instruction — and the deopt reproduces the interpreter exactly.
    let got = agree_comparison(&module, "eq_instr_arity3", 2, &[make_small(6), make_small(6)], || {
        comparison_floor(atoms.clone())
    });
    assert_eq!(got.ok(), Some(boolean(&module, true)), "eq_instr_arity3");

    // A float-register operand (the register file is `NIL`-filled): refused at compile
    // time, and the helper's operand read is the engine's exact semantics.
    let got = agree_comparison(&module, "lt_floatreg", 1, &[make_small(2)], || {
        comparison_floor(atoms.clone())
    });
    assert!(got.is_ok(), "lt_floatreg must be an answer, not a raise: {got:?}");

    // An unresolvable static literal: the helper's operand read is the engine's typed
    // error, and both tiers report it alike (never a panic, never a fabricated result).
    let got = agree_comparison(&module, "lt_literal", 1, &[make_small(2)], || {
        comparison_floor(atoms.clone())
    });
    assert!(got.is_err(), "an unresolvable literal must be a typed error: {got:?}");

    // A bignum constant: refused by the shape test, yet the shape still answers.
    let got = agree_comparison(&module, "lt_bigconst", 1, &[make_small(2)], || {
        comparison_floor(atoms.clone())
    });
    assert_eq!(got.ok(), Some(boolean(&module, true)), "2 < a bignum constant");

    // The ordering operators with an atom or `nil` constant are refused too (a
    // non-number is not word-ordered), and the shape still answers.
    for export in ["lt_atom", "lt_nil"] {
        for arg in [make_small(2), NIL] {
            let got = agree_comparison(&module, export, 1, &[arg], || comparison_floor(atoms.clone()));
            assert!(got.is_ok(), "{export} must be an answer, not a raise: {got:?}");
        }
    }

    // A `Y` operand and a `Y` destination (the helper's operand read/write is the
    // engine's typed error, since the fixture allocates no frame) and a destination
    // past `MAX_REGISTER`.
    for (export, arity, args) in [
        ("lt_y_operand", 2, vec![make_small(2), make_small(3)]),
        ("lt_y_dst", 1, vec![make_small(2)]),
        ("lt_dst_oob", 2, vec![make_small(2), make_small(3)]),
    ] {
        let got = agree_comparison(&module, export, arity, &args, || comparison_floor(atoms.clone()));
        assert!(got.is_err(), "{export} must be a typed error: {got:?}");
    }
}

#[test]
fn a_comparison_over_a_module_without_booleans_stays_on_the_helper() {
    // The result words are the *module's* `true`/`false` atoms, so a module whose atom
    // table lacks them cannot be lowered natively — the instruction must stay on the
    // helper. A context with no BIF floor surfaces the same typed error on every tier.
    let module = comparison_bif_module_without_booleans(
        "cmpbif_no_bools",
        &[CmpFn::new(
            "lt",
            2,
            "<",
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(0),
        )],
    );

    let args = [make_small(3), make_small(4)];
    let interpreted = agree_module(&module, "lt", 2, &args);
    assert!(interpreted.is_err(), "the fixture has no BIF floor: {interpreted:?}");

    // With a floor whose atom table *does* carry `true`/`false`, both tiers answer
    // through the helper and agree: the instruction is not lowered natively, so the
    // BIF hook's answer round-trips through `call_bif` on every tier.
    let mut atoms = module.atom_table.clone();
    atoms.intern("true");
    atoms.intern("false");
    let got = agree_comparison(&module, "lt", 2, &args, || comparison_floor(atoms.clone()));
    assert_eq!(
        got.ok(),
        Some(make_atom(atom_of(&atoms, "true"))),
        "lt(3, 4) through the helper"
    );
}

// ---------------------------------------------------------------------------
// 3. The inlining evidence
// ---------------------------------------------------------------------------

/// Run `export/arity` on `tier` behind a counting comparison floor, returning both the
/// outcome and the number of times the floor was invoked.
fn run_counted(
    tier: Tier,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    atoms: beam_term::AtomTable,
) -> (Result<Eterm, BeamError>, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let outcome = run_hooked(
        tier,
        module,
        export,
        arity,
        args,
        counting_comparison_floor(atoms, Arc::clone(&calls)),
    );
    (outcome, calls.load(Ordering::SeqCst))
}

#[test]
fn the_inlined_comparison_never_consults_the_bif_hook() {
    // Behavioural proof, not an inference: for the same program the interpreter steps
    // the BIF exactly once, so the baseline tier's *zero* calls can only mean the
    // inlined fast path computed the tagged word itself.
    let module = comparison_bif_fixtures();
    let atoms = module.atom_table.clone();
    let args = [make_small(3), make_small(4)];

    // Every operator's two-register form: interpreter = one BIF call, baseline = zero.
    for &(op, regs, ..) in &COMPARISONS {
        let (interpreted, interpreted_calls) =
            run_counted(Tier::Interpreter, &module, regs, 2, &args, atoms.clone());
        assert_eq!(
            interpreted.ok(),
            Some(boolean(&module, comparison_oracle(op, 3, 4))),
            "{op} via the interpreter"
        );
        assert_eq!(
            interpreted_calls, 1,
            "the interpreter must call {op} exactly once"
        );

        let (baseline, baseline_calls) =
            run_counted(Tier::BaselineJit, &module, regs, 2, &args, atoms.clone());
        assert_eq!(
            baseline.ok(),
            Some(boolean(&module, comparison_oracle(op, 3, 4))),
            "{op} via the baseline tier"
        );
        assert_eq!(
            baseline_calls, 0,
            "the inlined {op} fast path must not call the BIF"
        );
    }

    // The guard-free constant forms and the `gc_bif` spelling inline too.
    for (export, arity, args, expected) in [
        ("arith_eq_atom", 1, vec![make_small(7)], false),
        ("exact_eq_nil", 1, vec![NIL], true),
        ("gc_eq", 2, vec![make_small(7), make_small(7)], true),
    ] {
        let (baseline, calls) =
            run_counted(Tier::BaselineJit, &module, export, arity, &args, atoms.clone());
        assert_eq!(baseline.ok(), Some(boolean(&module, expected)), "{export}");
        assert_eq!(calls, 0, "the inlined {export} fast path must not call the BIF");
    }

    // A run-time guard rejection *does* reach the floor (a boxed float is not a tagged
    // small), so the zero above is the fast path's own doing rather than a hook that is
    // never installed.
    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    let (baseline, calls) = run_counted(
        Tier::BaselineJit,
        &module,
        "lt",
        2,
        &[five_float, make_small(5)],
        atoms.clone(),
    );
    assert_eq!(baseline.ok(), Some(boolean(&module, false)), "lt(5.0, 5)");
    assert_eq!(calls, 1, "a rejected operand must deopt to the helper");

    // ... and a *refused shape* reaches the floor too (the same instruction stays on the
    // helper for the whole run).
    let helper_module = non_inlinable_comparison_module();
    let helper_atoms = helper_module.atom_table.clone();
    for (export, arity, args) in [
        ("divide", 2u32, vec![make_small(6), make_small(7)]),
        ("eq_foreign", 2, vec![make_small(6), make_small(7)]),
        ("lt_bigconst", 1, vec![make_small(2)]),
    ] {
        let (_, calls) = run_counted(
            Tier::BaselineJit,
            &helper_module,
            export,
            arity,
            &args,
            helper_atoms.clone(),
        );
        assert_eq!(calls, 1, "the refused shape {export} must run through the hook");
    }
}

#[test]
fn the_baseline_artifact_records_the_comparison_fast_path() {
    // Artifact-level corroboration (the baseline tier has no `CodegenReport`): a
    // comparison-only block that inlines the fast path touches its operand/destination
    // x-registers **natively** — the prologue caches them, so the artifact's
    // `highest_native_register` is set. A block whose only instruction is a refused
    // comparison touches nothing natively (`label` is a no-op, `return` deopts), so the
    // artifact records no native register at all.
    let compiled = BaselineJit::new()
        .compile(&comparison_bif_fixtures())
        .expect("compile the comparison-BIF fixtures");
    assert!(compiled.block_count() > 0);
    // `gt_dst_two` writes the result to `x(2)`, the highest register any fixture names.
    assert_eq!(
        compiled.highest_native_register(),
        Some(2),
        "the inlined comparison must touch its operands/destination natively"
    );
    assert!(
        !compiled.cached_registers().is_empty(),
        "the fast path's registers must be cached: {:?}",
        compiled.cached_registers()
    );

    let compiled = BaselineJit::new()
        .compile(&non_inlinable_comparison_module())
        .expect("compile the non-inlinable comparison fixtures");
    assert!(compiled.block_count() > 0);
    assert_eq!(
        compiled.highest_native_register(),
        None,
        "no non-inlinable shape may touch a register natively"
    );
    assert!(
        compiled.cached_registers().is_empty(),
        "no non-inlinable shape may cache a register: {:?}",
        compiled.cached_registers()
    );

    // A module whose atom table lacks `true`/`false` must also stay on the helper.
    let compiled = BaselineJit::new()
        .compile(&comparison_bif_module_without_booleans(
            "cmpbif_no_bools",
            &[CmpFn::new(
                "lt",
                2,
                "<",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            )],
        ))
        .expect("compile the boolean-free fixture");
    assert_eq!(
        compiled.highest_native_register(),
        None,
        "a module without `true`/`false` must stay on the helper"
    );
}

#[test]
fn the_baseline_artifact_runs_the_compiled_comparison_blocks_and_agrees() {
    // The compile-once / run-many [`JitBackend`] entry (`run_compiled` on a caller-held
    // artifact) must produce interpreter-identical results, and the inlined fast path
    // must still avoid the BIF hook there.
    let module = comparison_bif_fixtures();
    let mut backend = BaselineJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the comparison-BIF fixtures");
    let atoms = module.atom_table.clone();

    // Two small registers: zero hook calls (the inlined fast path).
    let calls = Arc::new(AtomicUsize::new(0));
    let got = run_compiled_hooked(
        &backend,
        &compiled,
        &module,
        "lt",
        2,
        &[make_small(3), make_small(4)],
        counting_comparison_floor(atoms.clone(), Arc::clone(&calls)),
    )
    .expect("lt(3, 4) via the compiled artifact");
    assert_eq!(got, boolean(&module, true));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the compiled fast path must not call the BIF"
    );

    // A guard rejection: exactly one hook call, interpreter-identical answer.
    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    let calls = Arc::new(AtomicUsize::new(0));
    let args = [five_float, make_small(5)];
    let got = run_compiled_hooked(
        &backend,
        &compiled,
        &module,
        "arith_eq",
        2,
        &args,
        counting_comparison_floor(atoms.clone(), Arc::clone(&calls)),
    )
    .expect("arith_eq(5.0, 5) via the compiled artifact");
    assert_eq!(got, boolean(&module, true));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a rejected operand deopts");

    // Cross-check a sweep of the two-register forms against the interpreter.
    for &(op, regs, ..) in &COMPARISONS {
        for (a, b) in [(0i64, 0i64), (1, -1), (MIN_SMALL, MAX_SMALL), (MAX_SMALL, MIN_SMALL)] {
            let args = [make_small(a), make_small(b)];
            let interpreted =
                run_hooked(Tier::Interpreter, &module, regs, 2, &args, comparison_floor(atoms.clone()));
            let compiled_out = run_compiled_hooked(
                &backend,
                &compiled,
                &module,
                regs,
                2,
                &args,
                comparison_floor(atoms.clone()),
            );
            assert_same(&compiled_out, &interpreted, &format!("compiled {op} {a} {b}"));
            assert_eq!(
                interpreted.ok(),
                Some(boolean(&module, comparison_oracle(op, a, b))),
                "{op}: {a} vs {b}"
            );
        }
    }
}
