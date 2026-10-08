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

//! End-to-end integration test over the **sibling loader fixture**
//! `beam_loader/tests/fixtures/coverage.beam`.
//!
//! Where the per-family suites (`tests/tuples_lists.rs`, `tests/funs.rs`,
//! `tests/exceptions.rs`, `tests/floats.rs`, `tests/maps.rs`,
//! `tests/bitstrings.rs`) drive one instruction family each over a purpose-built
//! fixture, this test proves the widened-IR interpreter works on a **single broad,
//! idiomatic module**: it loads one real `.beam` (produced by the system `erlc`,
//! OTP 29 / `erts-17.1`, the differential oracle) and runs *every* export through
//! [`Machine::run_export`], asserting the **exact** result.
//!
//! Expected values were derived from the real system ERTS — loading the same
//! `coverage.beam` under `erl` and calling each function, e.g.
//! `coverage:tuple_ops({1,2}) = {2,1,3}`. Nothing here is guessed.
//!
//! # What the module covers
//!
//! `coverage.erl` (read-only; owned by the sibling `beam_loader` node) exports:
//!
//! | export | Erlang | families / opcodes |
//! |---|---|---|
//! | `tuple_ops/1` | `{A,B}` / `{A,B,C}` / `element/2` | `select_tuple_arity`, `get_tuple_element`, `put_tuple2`, `bif element` |
//! | `list_ops/1` | recursive `[H*2 \| T]` | `get_list`, `put_list`, local `call`, `is_nil` |
//! | `make_adder/1` | `fun(X) -> X + N end` | `make_fun3` (+ the `z,3` `{alloc,[{words,1},{funs,1}]}` need form) |
//! | `apply_adder/2` | calls that closure | `call_fun2`, local `call` |
//! | `classify/1` | `case` over guards | `is_eq_exact`, `is_integer`, `is_ge` |
//! | `safe_div/2` | `try A / B of Q -> {ok, Q} catch error:badarith …` | `try`, `try_end`, `try_case`, `fconv`, `fdiv`, `fmove`, `put_tuple2`, `init_yregs` |
//! | `float_ops/2` | `Sum+Diff+Prod` | `gc_bif '+'/'-'/'*'` (typed number operands) |
//! | `map_ops/2` | `#{a=>1,b=>2}` updates + `maps:get/3` | `put_map_assoc`, `put_map_exact`, `get_map_elements` |
//! | `bits/1` | `<<Rest/binary, H:8>>` | `bs_start_match3`, `bs_match`, `bs_get_position`, `bs_get_tail`, `bs_set_position`, `bs_create_bin` |
//! | `module_info/0,1` | compiler-generated | `call_ext_only` (+ `move`) |
//!
//! # Static literals and the remaining unsupported record
//!
//! The `LitT` static literals (`Operand::Literal`) are **supported**: the interpreter
//! binds the running module's decoded literal area to the [`Machine`] for the run, so
//! the generic operand seam (`beam_engine::operand::read_operand`) resolves
//! `Literal(idx)` alongside `X`/`Y`/`FloatReg`/`Integer`/`Atom`/`Nil`/`Character`/
//! `Typed`. This is what lets two exports complete:
//!
//! * `map_ops/2` — its first instruction is
//!   `PutMapAssoc { source: Literal(#{a=>1,b=>2}), .. }`.
//! * `safe_div(A, 0)` — its first clause is
//!   `Move { src: Literal({error,division_by_zero}) }`.
//!
//! A `Literal` index the module does not define stays the typed
//! `unsupported("operand: literal")` raise (never a substituted term); see the
//! engine's `src/operand.rs` unit tests.
//!
//! `bits/1` builds `<<Rest/binary, H:8>>` via `bs_create_bin` with an **`append`
//! segment**, which the bitstring family now supports (a copy-rebuild; see
//! `HANDOFF_bs_init_writable.md`). The match side
//! (`bs_start_match3`/`bs_match`/`bs_get_position`) also executes, and the `<<>>`
//! input takes the no-match clause (`bs_set_position`/`bs_get_tail`).
//!
//! `safe_div([], B)` documents the family-raise path: `fconv`'s non-numeric source
//! raises `badarith` **through the catch machinery** (`$BADARITH0` → `next_catch`,
//! the engine's [`Machine::throw_error`](beam_engine::Machine)), so it lands in the
//! live `try` frame and the catch clause yields `{error, badarith}` — exactly the
//! oracle value. `safe_div(1, 0.0)` exercises the same path from `fdiv` (a
//! non-finite quotient, again with no fail label).
//!
//! The module has no `receive`, no records and no message opcodes, so those families
//! are simply absent from its IR (not "unsupported").
//!
//! # Context
//!
//! [`CoverageCtx`] is the execution context. It **reuses**
//! [`MinimalContext`](beam_engine::MinimalContext)'s pointer-stable `alloc_words`
//! arena (Prevent-Duplication) but implements the dispatch hooks itself so that a
//! BIF hook can also **build** boxed results — `float_ops/2`'s
//! `gc_bif '+'/'-'/'*'` must return boxed floats for float arguments, which the
//! `MinimalContext` hook signature (`&[Eterm] -> Eterm`, no heap access) cannot do.
//! The dispatch is the module's `erlang:'+'/2`, `'-'/2`, `'*'/2`, `element/2` and
//! `get_module_info/1,2` plus the single `FunT` lambda `fun(X) -> X + N end`.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_engine::{ExecutionContext, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm};

/// The sibling `beam_loader` oracle fixture (reused by relative path — single
/// source of truth for the `.beam` image; never copied).
const COVERAGE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/coverage.beam"
);

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// A number, as the arithmetic BIFs see it.
#[derive(Clone, Copy)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

/// Interpret a term as a number: a small integer or a boxed float, else `None`.
fn number(t: Eterm) -> Option<Number> {
    if is_small(t) {
        return Some(Number::Int(signed_value(t)));
    }
    terms::float_value(t).ok().map(Number::Float)
}

/// The execution context for the coverage module.
///
/// Owns a [`MinimalContext`] purely as the pointer-stable `alloc_words` arena; it
/// overrides `call_bif`/`apply_fun` so the hooks can allocate boxed terms.
struct CoverageCtx {
    arena: MinimalContext,
}

impl CoverageCtx {
    fn new() -> CoverageCtx {
        CoverageCtx {
            arena: MinimalContext::new(),
        }
    }
}

impl ExecutionContext for CoverageCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (module, function, arity) {
            ("erlang", "+", 2) => arith(self, args, '+'),
            ("erlang", "-", 2) => arith(self, args, '-'),
            ("erlang", "*", 2) => arith(self, args, '*'),
            ("erlang", "element", 2) => element(args),
            // The `module_info/0,1` forwarders name `erlang:get_module_info/1,2`.
            // That BIF is outside the M1 floor, so the hook *echoes* the argument
            // it forwarded (the module name for /1, the key for /2); the tests then
            // assert the echo, proving the `call_ext_only` argument convention.
            ("erlang", "get_module_info", 1) => args.first().copied().ok_or(BeamError::BadArg),
            ("erlang", "get_module_info", 2) => args.get(1).copied().ok_or(BeamError::BadArg),
            _ => Err(BeamError::BadArg),
        }
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        match terms::fun_index(fun)? {
            // The module's sole lambda, `'-make_adder/1-fun-0-'/2`, is
            // `fun(X) -> X + N end` with `N` captured as its single free variable.
            0 => {
                let free = terms::fun_free(fun)?;
                if args.len() != 1 || free.len() != 1 {
                    return Err(BeamError::BadArity);
                }
                arith(self, &[args[0], free[0]], '+')
            }
            _ => Err(BeamError::BadArg),
        }
    }
}

/// `+`/`-`/`*` over two numbers: integer arithmetic stays integer, any float
/// operand produces a boxed float (the ERTS `erts_arith` number tower, restricted
/// to the small-int/float cases this fixture reaches).
fn arith(ctx: &mut dyn ExecutionContext, args: &[Eterm], op: char) -> Result<Eterm> {
    if args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    let a = number(args[0]).ok_or(BeamError::BadArith)?;
    let b = number(args[1]).ok_or(BeamError::BadArith)?;
    match (a, b) {
        (Number::Int(x), Number::Int(y)) => Ok(make_small(match op {
            '+' => x + y,
            '-' => x - y,
            '*' => x * y,
            _ => return Err(BeamError::BadArg),
        })),
        (x, y) => {
            let (x, y) = (x.as_f64(), y.as_f64());
            let value = match op {
                '+' => x + y,
                '-' => x - y,
                '*' => x * y,
                _ => return Err(BeamError::BadArg),
            };
            terms::make_float(ctx, value)
        }
    }
}

/// `erlang:element/2` — the 1-based element of a tuple.
fn element(args: &[Eterm]) -> Result<Eterm> {
    if args.len() != 2 || !is_small(args[0]) {
        return Err(BeamError::BadArg);
    }
    let index = signed_value(args[0]);
    if index < 1 {
        return Err(BeamError::BadArg);
    }
    // A non-tuple or an out-of-range index is already a typed `BadArg`.
    terms::tuple_element(args[1], (index - 1) as usize)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The module-name atom (`coverage`).
fn coverage_atom(module: &Module) -> Eterm {
    make_atom(atom(module, "coverage"))
}

fn load() -> Module {
    load_file(COVERAGE_BEAM).expect("load coverage.beam")
}

/// Run `name/arity` on a fresh machine over `ctx`.
fn run(ctx: &mut CoverageCtx, module: &Module, name: &str, args: &[Eterm]) -> Result<Eterm> {
    let mut machine = Machine::new();
    machine.run_export(module, atom(module, name), args.len() as u32, args, ctx)
}

/// Assert `result` is the atom `name` of `module`.
fn assert_atom(module: &Module, result: Result<Eterm>, name: &str) {
    assert_eq!(
        result.expect("a successful run"),
        make_atom(atom(module, name)),
        "expected the atom {name}"
    );
}

/// Assert `result` equals the tuple of `elements`.
fn assert_tuple(ctx: &mut CoverageCtx, module: &Module, result: Result<Eterm>, elements: &[Eterm]) {
    let result = result.expect("a successful run");
    let expected = terms::make_tuple(ctx, elements).expect("build the expected tuple");
    let arity = terms::tuple_arity(result).expect("a tuple");
    assert_eq!(arity, elements.len(), "tuple arity");
    for (i, element) in elements.iter().enumerate() {
        assert_eq!(
            terms::tuple_element(result, i).expect("in-range element"),
            *element,
            "tuple element {i}"
        );
    }
    // `cmp` is the module's exact `=:=` ordering (`beam_term`).
    assert!(
        beam_term::cmp(result, expected, &module.atom_table).is_eq(),
        "tuple mismatch"
    );
}

// ---------------------------------------------------------------------------
// Module inventory
// ---------------------------------------------------------------------------

#[test]
fn module_exports_and_fun_table_are_as_expected() {
    let module = load();
    assert_eq!(module.atom_name(module.name), Some("coverage"));

    // Every export the compiler emitted, mirroring `beam_disasm:file/1`.
    for (name, arity) in [
        ("tuple_ops", 1),
        ("list_ops", 1),
        ("make_adder", 1),
        ("apply_adder", 2),
        ("classify", 1),
        ("safe_div", 2),
        ("float_ops", 2),
        ("map_ops", 2),
        ("bits", 1),
        ("module_info", 0),
        ("module_info", 1),
    ] {
        assert!(
            module.export(atom(&module, name), arity).is_some(),
            "{name}/{arity} must be exported"
        );
    }

    // The one `FunT` lambda `'-make_adder/1-fun-0-'/2` (index 0).
    assert_eq!(module.funs().len(), 1, "one lambda in the FunT chunk");
    let lambda = module.fun(0).expect("fun-table index 0");
    assert_eq!(lambda.arity, 2);
    assert_eq!(lambda.num_free, 1);
    assert_eq!(lambda.uniq, 3_533_555);
}

// ---------------------------------------------------------------------------
// tuples — `tuple_ops/1`
// ---------------------------------------------------------------------------

#[test]
fn tuple_ops_two_element_tuple_rotates_and_sums() {
    // coverage:tuple_ops({1,2}) = {2,1,3}  (select_tuple_arity -> label 4)
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).expect("tuple");
    let result = run(&mut ctx, &module, "tuple_ops", &[arg]);
    assert_tuple(
        &mut ctx,
        &module,
        result,
        &[make_small(2), make_small(1), make_small(3)],
    );
}

#[test]
fn tuple_ops_three_element_tuple_reverses_and_diffs() {
    // coverage:tuple_ops({1,2,3}) = {3,2,1,2}  (select_tuple_arity -> label 3)
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)])
        .expect("tuple");
    let result = run(&mut ctx, &module, "tuple_ops", &[arg]);
    assert_tuple(
        &mut ctx,
        &module,
        result,
        &[make_small(3), make_small(2), make_small(1), make_small(2)],
    );
}

#[test]
fn tuple_ops_other_arity_reads_the_first_element_via_the_element_bif() {
    // coverage:tuple_ops({5,6,7,8}) = 5  (arity 4 -> label 5 -> `bif element/2`)
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_tuple(
        &mut ctx,
        &[make_small(5), make_small(6), make_small(7), make_small(8)],
    )
    .expect("tuple");
    let result = run(&mut ctx, &module, "tuple_ops", &[arg]);
    assert_eq!(result.expect("tuple_ops/1"), make_small(5));
}

// ---------------------------------------------------------------------------
// lists — `list_ops/1`
// ---------------------------------------------------------------------------

#[test]
fn list_ops_doubles_every_element() {
    // coverage:list_ops([1,2,3]) = [2,4,6]
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_list(&mut ctx, &[make_small(1), make_small(2), make_small(3)])
        .expect("list");
    let result = run(&mut ctx, &module, "list_ops", &[arg]).expect("list_ops/1");
    let elements = terms::list_elements(result).expect("a proper list");
    assert_eq!(
        elements,
        vec![make_small(2), make_small(4), make_small(6)]
    );
}

#[test]
fn list_ops_of_the_empty_list_is_the_empty_list() {
    // coverage:list_ops([]) = []  (the `is_nil` clause)
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_list(&mut ctx, &[]).expect("empty list");
    let result = run(&mut ctx, &module, "list_ops", &[arg]).expect("list_ops([])");
    assert_eq!(result, beam_term::NIL);
}

// ---------------------------------------------------------------------------
// funs — `make_adder/1`, `apply_adder/2`
// ---------------------------------------------------------------------------

#[test]
fn make_adder_builds_a_closure_capturing_one_free_variable() {
    // coverage:make_adder(10) = #Fun<coverage.0.3533555> — fun-table index 0 and
    // uniq 3533555 (from the oracle); the header arity is 2 - 1 = 1.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let fun = run(&mut ctx, &module, "make_adder", &[make_small(10)]).expect("make_adder/1");
    assert!(terms::is_fun(fun), "make_adder/1 returns a fun");
    assert_eq!(terms::fun_index(fun).unwrap(), 0);
    assert_eq!(terms::fun_uniq(fun).unwrap(), 3_533_555);
    assert_eq!(terms::fun_arity(fun).unwrap(), 1);
    assert_eq!(terms::fun_num_free(fun).unwrap(), 1);
    assert_eq!(terms::fun_free(fun).unwrap(), &[make_small(10)]);
}

#[test]
fn apply_adder_applies_the_captured_closure() {
    // coverage:apply_adder(10, 5) = 15  (make_adder then call_fun2)
    let module = load();
    let mut ctx = CoverageCtx::new();
    let result = run(
        &mut ctx,
        &module,
        "apply_adder",
        &[make_small(10), make_small(5)],
    )
    .expect("apply_adder/2");
    assert_eq!(result, make_small(15));
}

// ---------------------------------------------------------------------------
// case / guards — `classify/1`
// ---------------------------------------------------------------------------

#[test]
fn classify_matches_each_case_clause() {
    // coverage:classify/1 — the four guards. Derived from the oracle:
    // classify(0) = zero, classify(5) = positive, classify(-3) = negative,
    // classify(1.5) = other (a non-integer is neither of the integer guards).
    let module = load();
    let mut ctx = CoverageCtx::new();

    assert_atom(
        &module,
        run(&mut ctx, &module, "classify", &[make_small(0)]),
        "zero",
    );
    assert_atom(
        &module,
        run(&mut ctx, &module, "classify", &[make_small(5)]),
        "positive",
    );
    assert_atom(
        &module,
        run(&mut ctx, &module, "classify", &[make_small(-3)]),
        "negative",
    );
    let non_integer = terms::make_float(&mut ctx, 1.5).expect("float");
    assert_atom(
        &module,
        run(&mut ctx, &module, "classify", &[non_integer]),
        "other",
    );
}

// ---------------------------------------------------------------------------
// float arithmetic — `float_ops/2`
// ---------------------------------------------------------------------------

#[test]
fn float_ops_over_floats_computes_two_a_plus_ab() {
    // coverage:float_ops(2.0, 3.0) = (2+3) + (2-3) + (2*3) = 2*2 + 2*3 = 10.0
    let module = load();
    let mut ctx = CoverageCtx::new();
    let a = terms::make_float(&mut ctx, 2.0).expect("float");
    let b = terms::make_float(&mut ctx, 3.0).expect("float");
    let result = run(&mut ctx, &module, "float_ops", &[a, b]).expect("float_ops/2 over floats");
    assert_eq!(terms::float_value(result).expect("a float"), 10.0);
    // `cmp` is the exact `=:=` ordering: two boxed floats are value-equal when they
    // hold the same `f64` (bitwise `Eterm` equality would compare pointers).
    let expected = terms::make_float(&mut ctx, 10.0).expect("float");
    assert!(beam_term::cmp(result, expected, &module.atom_table).is_eq());
}

#[test]
fn float_ops_over_integers_stays_integral() {
    // coverage:float_ops(2, 3) = 10  (the same `gc_bif` arithmetic, integer tower)
    let module = load();
    let mut ctx = CoverageCtx::new();
    let result = run(
        &mut ctx,
        &module,
        "float_ops",
        &[make_small(2), make_small(3)],
    )
    .expect("float_ops/2 over integers");
    assert_eq!(result, make_small(10));
}

// ---------------------------------------------------------------------------
// bitstrings — `bits/1`
// ---------------------------------------------------------------------------

#[test]
fn bits_rotates_the_leading_byte_to_the_end() {
    // coverage:bits(<<1,2,3>>) = <<2,3,1>>. The interpreter runs the whole match
    // side (`bs_start_match3`, `bs_get_position`, `bs_match`) to split off the
    // leading byte `H = 1` and the tail `Rest = <<2,3>>`, then the clause body's
    // `bs_create_bin` `<append, Rest/binary, H:8>` segment rebuilds `<<2,3,1>>`.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_heap_binary(&mut ctx, &[1, 2, 3]).expect("binary");
    let result = run(&mut ctx, &module, "bits", &[arg]).expect("bits(<<1,2,3>>)");
    assert_eq!(
        terms::binary_bytes(result).expect("a binary"),
        &[2, 3, 1],
        "bits/1 rotates the leading byte to the end"
    );
}

#[test]
fn bits_of_a_single_byte_is_unchanged() {
    // coverage:bits(<<7>>) = <<7>> (H = 7, Rest = <<>>): the `append` segment is
    // seeded with the empty tail and then writes the leading byte back.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_heap_binary(&mut ctx, &[7]).expect("binary");
    let result = run(&mut ctx, &module, "bits", &[arg]).expect("bits(<<7>>)");
    assert_eq!(
        terms::binary_bytes(result).expect("a binary"),
        &[7],
        "bits/1 on a single byte leaves it unchanged"
    );
}

#[test]
fn bits_of_the_empty_binary_takes_the_no_match_clause() {
    // coverage:bits(<<>>) = <<>>. The first clause's `bs_match` (`ensure_at_least 8`)
    // fails on the empty subject and jumps to label 30 (`bs_set_position` +
    // `bs_get_tail` + return), which returns the subject unchanged — so this input
    // never reaches `bs_create_bin` and is fully supported.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let arg = terms::make_heap_binary(&mut ctx, &[]).expect("empty binary");
    let result = run(&mut ctx, &module, "bits", &[arg]).expect("bits(<<>>)");
    assert_eq!(terms::binary_bytes(result).expect("a binary"), &[]);
}

// ---------------------------------------------------------------------------
// exceptions + floats — `safe_div/2`
// ---------------------------------------------------------------------------

#[test]
fn safe_div_non_zero_divisor_returns_ok_with_the_float_quotient() {
    // coverage:safe_div(7, 2) = {ok, 3.5}
    //
    // `B =/= 0` skips the first clause's literal and runs the `try` body:
    // `try` -> `fconv x0->fr0`, `fconv x1->fr1`, `fdiv fr0,fr1->fr0`, `try_end`,
    // `fmove fr0->x0`, `put_tuple2 {ok, x0}`. That exercises the exception frame
    // (try/try_end) and the whole float instruction family end to end.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let result = run(
        &mut ctx,
        &module,
        "safe_div",
        &[make_small(7), make_small(2)],
    )
    .expect("safe_div(7, 2)");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(
        terms::tuple_element(result, 0).unwrap(),
        make_atom(atom(&module, "ok"))
    );
    assert_eq!(
        terms::float_value(terms::tuple_element(result, 1).unwrap()).expect("a float quotient"),
        3.5
    );
}

#[test]
fn safe_div_zero_divisor_returns_the_literal_error_tuple() {
    // coverage:safe_div(1, 0) = {error, division_by_zero} (system ERTS, OTP 29.1.1).
    //
    // `is_eq_exact x1 = 0` takes the first clause, which compiles the constant tuple
    // to `move {literal,{error,division_by_zero}},{x,0}` — a static literal now read
    // through the machine's bound literal area (the `{error,badarith}` literal of the
    // catch clause, label 21, is not reached on this path).
    let module = load();
    let mut ctx = CoverageCtx::new();
    let result = run(&mut ctx, &module, "safe_div", &[make_small(1), make_small(0)])
        .expect("safe_div(1, 0)");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(
        terms::tuple_element(result, 0).unwrap(),
        make_atom(atom(&module, "error"))
    );
    assert_eq!(
        terms::tuple_element(result, 1).unwrap(),
        make_atom(atom(&module, "division_by_zero"))
    );
}

#[test]
fn safe_div_badarith_from_fconv_lands_in_the_catch_clause() {
    // coverage:safe_div([], 2) = {error,badarith} (system ERTS, OTP 29.1.1 /
    // erts-17.1). `is_eq_exact x1 = 0` fails (2 =/= 0), so the `try` body runs:
    // `fconv x0 -> fr0` on the non-number `[]` raises `badarith` with no fail label
    // (`$BADARITH0` → `find_func_info` → `next_catch`), landing in the live catch
    // frame at label 21. The handler matches `error:badarith` and yields the literal
    // `{error,badarith}`.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let result = run(
        &mut ctx,
        &module,
        "safe_div",
        &[beam_term::NIL, make_small(2)],
    );
    assert_tuple(
        &mut ctx,
        &module,
        result,
        &[
            make_atom(atom(&module, "error")),
            make_atom(atom(&module, "badarith")),
        ],
    );
}

#[test]
fn safe_div_badarith_from_a_non_finite_fdiv_lands_in_the_catch_clause() {
    // coverage:safe_div(1, 0.0) = {error,badarith} (system ERTS). A *float* divisor
    // is not `=:= 0`, so the `try` body runs: both `fconv`s succeed, then
    // `fdiv 1.0/0.0` overflows to `+inf`, the `$FLOAT_OP` finiteness check fails with
    // no fail label, and the `badarith` raise lands in the catch clause — the float
    // family's non-finite path, catchable exactly as C's `$BADARITH0` is.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let one = terms::make_float(&mut ctx, 1.0).expect("a float");
    let zero = terms::make_float(&mut ctx, 0.0).expect("a float");
    let result = run(&mut ctx, &module, "safe_div", &[one, zero]);
    assert_tuple(
        &mut ctx,
        &module,
        result,
        &[
            make_atom(atom(&module, "error")),
            make_atom(atom(&module, "badarith")),
        ],
    );
}

// ---------------------------------------------------------------------------
// maps — `map_ops/2`
// ---------------------------------------------------------------------------

#[test]
fn map_ops_reads_the_literal_source_map_and_returns_the_updated_key() {
    // The module's first instruction is
    // `put_map_assoc {f,0},{literal,#{a=>1,b=>2}},{x,2},2,[{atom,c},{x,1}]`, so this
    // export exercises the static-literal operand path end to end.
    //
    // Oracle-derived values (system ERTS, OTP 29.1.1 / erts-17.1):
    //   coverage:map_ops(a, 42) = 52   (M2 = #{a=>52,b=>2,c=>42}; `a` was updated)
    //   coverage:map_ops(b, 42) = 2    (untouched by either update)
    //   coverage:map_ops(c, 42) = 42   (inserted by the first update)
    //   coverage:map_ops(99, 42) = 0   (the `maps:get/3` default clause, label 27)
    let module = load();
    let mut ctx = CoverageCtx::new();
    for (key, expected) in [
        (make_atom(atom(&module, "a")), 52),
        (make_atom(atom(&module, "b")), 2),
        (make_atom(atom(&module, "c")), 42),
        (make_small(99), 0),
    ] {
        let result = run(&mut ctx, &module, "map_ops", &[key, make_small(42)])
            .expect("map_ops/2 must run");
        assert_eq!(result, make_small(expected), "map_ops(key, 42) for {key:?}");
    }
}

// ---------------------------------------------------------------------------
// external calls — `module_info/0,1`
// ---------------------------------------------------------------------------

#[test]
fn module_info_zero_forwards_the_module_name_to_get_module_info_one() {
    // `module_info/0` is `move {atom,coverage},{x,0}` + `call_ext_only 1,
    // {extfunc,erlang,get_module_info,1}`. The hook echoes x0, so the result is the
    // module-name atom — proving the ext-call argument convention end to end.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let result = run(&mut ctx, &module, "module_info", &[]).expect("module_info/0");
    assert_eq!(result, coverage_atom(&module));
}

#[test]
fn module_info_one_forwards_the_key_to_get_module_info_two() {
    // `module_info(Key)` is `move {x,0},{x,1}` + `move {atom,coverage},{x,0}` +
    // `call_ext_only 2, {extfunc,erlang,get_module_info,2}`. The hook echoes x1 (the
    // key), which is the atom passed in.
    let module = load();
    let mut ctx = CoverageCtx::new();
    let key = make_atom(atom(&module, "module_info"));
    let result = run(&mut ctx, &module, "module_info", &[key]).expect("module_info/1");
    assert_eq!(result, key);
}

// ---------------------------------------------------------------------------
// The label map is module-wide (calls cross functions)
// ---------------------------------------------------------------------------

#[test]
fn the_ir_has_module_wide_labels_across_functions() {
    // A guard on the fixture's shape: the interpreter builds a module-wide
    // `Label -> (function, code)` map, so the cross-function calls the exports make
    // (`list_ops/1` recursion, `apply_adder/2` -> `make_adder/1`) resolve. Several
    // functions' labels are present, which is what makes that map necessary.
    let module = load();
    assert!(
        module.functions.len() >= 6,
        "coverage.beam groups several functions"
    );
    let with_labels = module
        .functions
        .iter()
        .filter(|function| !function.labels.is_empty())
        .count();
    assert!(
        with_labels >= 2,
        "labels are spread across more than one function"
    );
}
