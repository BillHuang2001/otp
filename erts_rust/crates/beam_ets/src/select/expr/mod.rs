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

//! Guard and body expressions: decode a match-spec expression term into an
//! [`Expr`] tree and evaluate it against a clause's bindings.
//!
//! Mirrors the C `dmc_expr`/`dmc_fun`/`guard_tab` dispatch
//! (`../../erts/emulator/beam/erl_db_util.c`): a tuple is a **function call**
//! when its first element is a non-variable atom, or the `{{ … }}` construct form
//! when it is a one-element tuple holding a tuple; a list builds a list value; and
//! anything else is a constant. Comparison delegates to `beam_term::compare`
//! (`cmp` for the exact operators, `cmp_arith` for the arithmetic/ordinal ones) —
//! term order is never re-implemented here.
//!
//! The evaluation helpers that do not depend on the compiler (the guard BIFs, the
//! term-class probes and the boolean/atom conversions) live in the sibling
//! [`guard`] module; this module owns the AST, its decoding and the evaluator.
//!
//! # Arithmetic (the [`ArithHook`] seam)
//!
//! `+ - * / div rem band bor bxor bsl bsr` are accepted in **both** guards and
//! bodies, binary as well as unary for `+`/`-` (`abs/1` stays the separate
//! [`Expr::Abs`]). The C `guard_tab` maps them straight onto the ordinary BIFs —
//! `erl_db_util.c:887-894` binds `bsl`/`bsr` to `bsl_2`/`bsr_2` — so a match spec
//! sees exactly what the corresponding Erlang expression worker sees.
//!
//! Evaluation goes through the [`ArithHook`] seam rather than a hard-coded
//! implementation: a runtime that owns richer arithmetic installs its own hook
//! with [`set_arith_hook`], and without one [`DefaultArithHook`] answers.
//! The default implements the **integer** operators exactly and the **float**
//! operators faithfully (`arith`): a small-integer `+`/`-`/`*` keeps the checked
//! `i64` path, a **float** operand moves the operation into `f64` (a bignum
//! converting through `big_to_f64`), and `/` is **always** a float (`1/2` is
//! `0.5`, `4/2` is `2.0`). It never fabricates a result — a bignum operand on an
//! integer operator, an `i64` overflow, and a result outside the 60-bit small
//! range are the typed `unsupported_ets` raise, a float overflow to `±inf`/`NaN`
//! is refused, and `div`/`rem` by zero, a zero `/` divisor and a non-number
//! operand are `badarg` (what the oracle raises).
//!
//! Shift operators follow the C `erts_shift`
//! (`../../erts/emulator/beam/erl_arith.c:196-247`) exactly over the small-integer
//! subset: a **negative** count *reverses* the direction (the `bsr` count is
//! negated and the sign of the resulting count picks the direction, so `bsl`/`bsr`
//! merely swap — `8 bsr -1` is `16`, `1 bsl -1` is `0`), and a **right** shift by
//! 59 or more saturates to the sign's `0`/`-1` (`-1 bsr 100` is `-1`). A **left**
//! shift that leaves the 60-bit small range (`2 bsl 62`, `1 bsl 100`) is a bignum
//! in the oracle and stays the typed `unsupported`.
//!
//! Like every other guard construct, arithmetic that raises inside a **guard**
//! makes its clause fail and is skipped by the caller; the body path is the
//! executor's (`select/exec.rs`).
//!
//! # `self/0` (the [`SelfHook`] seam)
//!
//! `self/0` has no process identity at the ETS layer. The seam mirrors
//! [`ArithHook`]: a runtime that knows the calling process installs its own
//! [`SelfHook`] with [`set_self_hook`], and without one the default answers the
//! typed `badarg`, so a `self/0` guard clause is skipped exactly as before.
//!
//! # The extended guard BIFs
//!
//! `ceil/1`, `floor/1`, `max/2`, `min/2`, `binary_part/2,3`, `is_record/3`,
//! `is_function/2`, `is_integer/3`, `node/0,1`, `xor/2` and `bnot/1` decode to
//! their own [`Expr`] variants here and are evaluated by [`guard`] — mirroring the
//! same `guard_tab` dispatch, with a typed error (never a fabricated answer) for
//! every operand outside the implemented subset.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::{atom_name, var_index};
use crate::error::{badarg, unsupported};

mod arith;
mod guard;

use arith::default_arith;
pub use guard::{set_self_hook, SelfHook};

/// A comparison operator usable in a guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=:=` (exact equality).
    ExactEq,
    /// `=/=` (exact inequality).
    ExactNe,
    /// `==` (arithmetic equality).
    ArithEq,
    /// `/=` (arithmetic inequality).
    ArithNe,
    /// `<`.
    Lt,
    /// `=<`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// A type test usable in a guard (`is_atom/1`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTest {
    /// `is_atom/1`.
    Atom,
    /// `is_integer/1`.
    Integer,
    /// `is_float/1`.
    Float,
    /// `is_number/1`.
    Number,
    /// `is_tuple/1`.
    Tuple,
    /// `is_list/1`.
    List,
    /// `is_binary/1`.
    Binary,
    /// `is_bitstring/1`.
    Bitstring,
    /// `is_map/1`.
    Map,
    /// `is_pid/1`.
    Pid,
    /// `is_port/1`.
    Port,
    /// `is_reference/1`.
    Reference,
    /// `is_function/1` (any arity).
    Function,
    /// `is_boolean/1`.
    Boolean,
}

/// An arithmetic operator usable in a guard or a body.
///
/// All eleven spellings are accepted in **both** guards and bodies (verified
/// against the system `erl`); a **unary** `+`/`-` (`{'+', A}`/`{'-', A}`) reuses
/// [`Add`](ArithOp::Add)/[`Sub`](ArithOp::Sub) with a one-element operand vector.
/// `abs/1` is *not* here — it is the separate [`Expr::Abs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+` — binary addition, or unary plus with one operand.
    Add,
    /// `-` — binary subtraction, or unary minus with one operand.
    Sub,
    /// `*`.
    Mul,
    /// `/` — **always** a float result in Erlang (`1/2` is `0.5`, `4/2` is `2.0`).
    Div,
    /// `div`.
    IntDiv,
    /// `rem`.
    Rem,
    /// `band`.
    Band,
    /// `bor`.
    Bor,
    /// `bxor`.
    Bxor,
    /// `bsl`.
    Bsl,
    /// `bsr`.
    Bsr,
}

/// The evaluation seam for match-spec arithmetic (see the module docs).
///
/// [`eval`] takes no hook parameter — the executor in `select/exec.rs` calls it
/// directly — so the hook is installed **per thread** with [`set_arith_hook`]
/// instead of being threaded through every expression. A runtime that owns richer
/// arithmetic installs its own implementation there; without one
/// [`DefaultArithHook`] answers, and it implements the integer and float
/// operators in [`arith`](self::arith).
pub trait ArithHook {
    /// Evaluate one arithmetic operation over its already-evaluated operands.
    ///
    /// `args` holds the operand terms (one for a unary `+`/`-`, two otherwise;
    /// [`decode`] rejects every other arity with `badarg`). `heap` is the caller's
    /// heap, for an implementation that has to materialize a term (a float, say).
    fn eval(
        &self,
        op: ArithOp,
        args: &[Eterm],
        _atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Eterm> {
        default_arith(op, args, heap)
    }
}

/// The built-in arithmetic hook: integer `+ - * div rem band bor bxor bsl bsr`
/// (and unary `+`/`-`) via checked `i64` arithmetic, plus faithful **float**
/// `+ - * /` (a boxed float result).
///
/// Every case outside that subset is a typed error rather than a fabricated
/// answer — see [`default_arith`], which lists exactly which failure each case
/// maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultArithHook;

impl ArithHook for DefaultArithHook {}

thread_local! {
    /// The arithmetic hook installed on this thread; `None` means
    /// [`DefaultArithHook`] (see [`set_arith_hook`]).
    static ARITH_HOOK: RefCell<Option<Rc<dyn ArithHook + 'static>>> = RefCell::new(None);
}

/// Install `hook` as the arithmetic hook of the **current thread**; `None`
/// restores [`DefaultArithHook`].
///
/// Process-local by construction: every table op runs on the calling thread, and
/// `beam_rt` installs the hook for the process it is running.
pub fn set_arith_hook(hook: Option<Rc<dyn ArithHook + 'static>>) {
    ARITH_HOOK.with(|cell| *cell.borrow_mut() = hook);
}

/// The arithmetic hook in force on this thread: the installed one, else
/// [`DefaultArithHook`].
///
/// The `Rc` is **cloned out** of the `RefCell` before it is returned, so no borrow
/// is ever held across an [`eval`] call — the hook may itself evaluate
/// expressions.
fn arith_hook() -> Rc<dyn ArithHook + 'static> {
    ARITH_HOOK.with(|cell| {
        cell.borrow()
            .clone()
            .unwrap_or_else(|| Rc::new(DefaultArithHook))
    })
}

/// One guard/body expression node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A constant literal term.
    Literal(Eterm),
    /// `'$N'` — the value bound to variable `N` by the head.
    Bind(u32),
    /// `'$_'` — the whole matched object.
    WholeObject,
    /// `'$$'` — every bound head variable, ascending (computed at compile time).
    AllBindings(Vec<u32>),
    /// A fresh tuple built from the parts.
    Tuple(Vec<Expr>),
    /// A fresh cons cell built from head/tail.
    Cons(Box<Expr>, Box<Expr>),
    /// `and` — every operand is evaluated and each must be `true` (strict: a
    /// later operand is still evaluated after a `false`, so it may still raise).
    And(Vec<Expr>),
    /// `andalso` — operands are evaluated left to right and the first `false`
    /// ends the evaluation with `false`.
    AndAlso(Vec<Expr>),
    /// `or` — every operand is evaluated and at least one must be `true`
    /// (strict).
    Or(Vec<Expr>),
    /// `orelse` — operands are evaluated left to right and the first `true`
    /// ends the evaluation with `true`.
    OrElse(Vec<Expr>),
    /// `not` — the operand must be a boolean.
    Not(Box<Expr>),
    /// A comparison operator.
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// A type test.
    Test(TypeTest, Box<Expr>),
    /// `element/2`.
    Element(Box<Expr>, Box<Expr>),
    /// `abs/1`.
    Abs(Box<Expr>),
    /// An arithmetic operator over its operands (`+ - * / div rem band bor bxor
    /// bsl bsr`, with a one-element operand vector for unary `+`/`-`). Evaluated
    /// through the [`ArithHook`] seam, never inline.
    Arith(ArithOp, Vec<Expr>),
    /// `size/1`.
    Size(Box<Expr>),
    /// `hd/1`.
    Hd(Box<Expr>),
    /// `tl/1`.
    Tl(Box<Expr>),
    /// `length/1`.
    Length(Box<Expr>),
    /// `map_size/1`.
    MapSize(Box<Expr>),
    /// `map_get/2` — `(Key, Map)`; an absent key raises `badarg` (the oracle).
    MapGet(Box<Expr>, Box<Expr>),
    /// `is_map_key/2` — `(Key, Map)`; a non-map raises `badarg`.
    IsMapKey(Box<Expr>, Box<Expr>),
    /// `tuple_size/1`.
    TupleSize(Box<Expr>),
    /// `byte_size/1` — the byte count of a bitstring (`NBYTES(size_in_bits)`).
    ByteSize(Box<Expr>),
    /// `bit_size/1` — the bit count of a bitstring.
    BitSize(Box<Expr>),
    /// `float/1`.
    Float(Box<Expr>),
    /// `trunc/1`.
    Trunc(Box<Expr>),
    /// `round/1`.
    Round(Box<Expr>),
    /// A map built from key/value expression pairs (`#{K => V, …}` in a guard or a
    /// body — the C `dmc_map`). The pairs are in the spec's storage order; the map is
    /// materialised by `beam_term::map::write_map`, which canonicalises them.
    Map(Vec<(Expr, Expr)>),
    /// `self/0`.
    SelfPid,
    /// An internal boolean literal. Never produced by [`decode`] (a literal
    /// `true`/`false` decodes to [`Expr::Literal`] so it still renders as its
    /// atom); the pattern-only builders whose body is `[true]` (`match_delete`)
    /// use it so the deletion test does not depend on the `AtomTable`.
    Bool(bool),
    /// `ceil/1`.
    Ceil(Box<Expr>),
    /// `floor/1`.
    Floor(Box<Expr>),
    /// `max/2` — the larger operand by **term order** (`cmp_arith`).
    Max(Box<Expr>, Box<Expr>),
    /// `min/2` — the smaller operand by **term order** (`cmp_arith`).
    Min(Box<Expr>, Box<Expr>),
    /// `binary_part/2` — the second operand is a **runtime** `{Pos, Len}` 2-tuple.
    BinaryPart2(Box<Expr>, Box<Expr>),
    /// `binary_part/3`.
    BinaryPart3(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `is_record/3`.
    IsRecord3(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `is_function/2`.
    IsFunction2(Box<Expr>, Box<Expr>),
    /// `is_integer/3`.
    IsInteger3(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `node/0`.
    Node0,
    /// `node/1`.
    Node1(Box<Expr>),
    /// `xor/2`.
    Xor(Box<Expr>, Box<Expr>),
    /// `bnot/1`.
    Bnot(Box<Expr>),
}

/// The result of evaluating a guard or body expression.
///
/// A comparison, a type test or a boolean connective produces an **internal**
/// boolean rather than an atom term, so whether a guard holds never depends on
/// the caller's [`AtomTable`] having interned `true`/`false`. Every other
/// expression produces an ordinary term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Value {
    /// An internal boolean (from a comparison, type test or connective).
    Bool(bool),
    /// A term-valued result.
    Term(Eterm),
}

impl Value {
    /// The term this value denotes. An internal boolean is rendered as the atom
    /// `true`/`false`, which must be interned in `atoms` (`badarg` otherwise).
    pub(super) fn into_term(self, atoms: &AtomTable) -> Result<Eterm> {
        match self {
            Value::Bool(flag) => guard::bool_term(flag, atoms),
            Value::Term(term) => Ok(term),
        }
    }
}

/// Decode an expression term. `vars` is the set of `'$N'` bound by the head; a
/// reference to any other variable is `badarg` (the C `dmc_variable` error).
pub(super) fn decode(term: Eterm, atoms: &AtomTable, vars: &BTreeSet<u32>) -> Result<Expr> {
    if let Some(name) = atom_name(term, atoms) {
        match name {
            "$_" => return Ok(Expr::WholeObject),
            "$$" => return Ok(Expr::AllBindings(vars.iter().copied().collect())),
            "_" => return Ok(Expr::Literal(term)),
            _ => {
                if let Some(index) = var_index(name) {
                    if !vars.contains(&index) {
                        return Err(badarg());
                    }
                    return Ok(Expr::Bind(index));
                }
                return Ok(Expr::Literal(term));
            }
        }
    }
    if let Some(elements) = super::tuple_elements(term) {
        return decode_tuple(term, &elements, atoms, vars);
    }
    if is_list(term) {
        // SAFETY: `is_list(term)` established a live cons cell, so its two
        // `[CAR, CDR]` words are readable; the spec term is caller-owned.
        let (head_term, tail_term) = unsafe { (car(term), cdr(term)) };
        let head = Box::new(decode(head_term, atoms, vars)?);
        let tail = Box::new(decode(tail_term, atoms, vars)?);
        return Ok(Expr::Cons(head, tail));
    }
    // A map expression is **constructed** (`dmc_map`): its keys/values may be
    // variables, so each is decoded as an expression and the map is materialised on
    // the caller's heap at evaluation time. Verified against the system `erl`: a map
    // literal is accepted in both a guard and a body
    // (`ets:select(T, [{{'$1','$2'},[], [#{'$1' => '$2'}]}])` yields a map per
    // matched object). A map **in a head** is still compared as an exact literal by
    // `pattern.rs` — that gap is documented, not changed here.
    if classify(term) == TermClass::Map {
        let mut pairs = Vec::new();
        for (key, value) in map_pairs(term, atoms).map_err(|_| badarg())? {
            pairs.push((decode(key, atoms, vars)?, decode(value, atoms, vars)?));
        }
        return Ok(Expr::Map(pairs));
    }
    Ok(Expr::Literal(term))
}

/// Decode a tuple expression term: a call, or the `{{ … }}` construct form.
fn decode_tuple(
    term: Eterm,
    elements: &[Eterm],
    atoms: &AtomTable,
    vars: &BTreeSet<u32>,
) -> Result<Expr> {
    // `{{ … }}` — a one-element tuple whose element is a tuple constructs that
    // tuple.
    if elements.len() == 1 {
        if let Some(inner) = super::tuple_elements(elements[0]) {
            let mut parts = Vec::with_capacity(inner.len());
            for element in inner {
                parts.push(decode(element, atoms, vars)?);
            }
            return Ok(Expr::Tuple(parts));
        }
    }
    // `{name, Arg…}` where `name` is a non-variable atom is a function call.
    if let Some(name) = atom_name(elements[0], atoms) {
        if var_index(name).is_none() {
            return decode_call(name, &elements[1..], atoms, vars);
        }
    }
    let _ = term;
    Err(badarg())
}

/// Decode a guard/body call by its name and arguments.
fn decode_call(name: &str, args: &[Eterm], atoms: &AtomTable, vars: &BTreeSet<u32>) -> Result<Expr> {
    let arg = |index: usize| -> Result<Expr> { decode(args[index], atoms, vars) };
    let pairwise = |op: CmpOp| -> Result<Expr> {
        if args.len() != 2 {
            return Err(badarg());
        }
        Ok(Expr::Cmp(op, Box::new(arg(0)?), Box::new(arg(1)?)))
    };
    let one = |test: TypeTest| -> Result<Expr> {
        if args.len() != 1 {
            return Err(badarg());
        }
        Ok(Expr::Test(test, Box::new(arg(0)?)))
    };
    let binary_arith = |op: ArithOp| -> Result<Expr> {
        if args.len() != 2 {
            return Err(badarg());
        }
        Ok(Expr::Arith(op, vec![arg(0)?, arg(1)?]))
    };
    let unary = |build: fn(Box<Expr>) -> Expr| -> Result<Expr> {
        if args.len() != 1 {
            return Err(badarg());
        }
        Ok(build(Box::new(arg(0)?)))
    };
    let binary = |build: fn(Box<Expr>, Box<Expr>) -> Expr| -> Result<Expr> {
        if args.len() != 2 {
            return Err(badarg());
        }
        Ok(build(Box::new(arg(0)?), Box::new(arg(1)?)))
    };
    match name {
        // Special forms.
        "andalso" | "and" | "orelse" | "or" => {
            if args.len() < 2 {
                return Err(badarg());
            }
            let mut parts = Vec::with_capacity(args.len());
            for index in 0..args.len() {
                parts.push(arg(index)?);
            }
            // `and`/`or` are strict; `andalso`/`orelse` short-circuit (verified
            // against `erl`).
            Ok(match name {
                "andalso" => Expr::AndAlso(parts),
                "and" => Expr::And(parts),
                "orelse" => Expr::OrElse(parts),
                _ => Expr::Or(parts),
            })
        }
        "not" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Not(Box::new(arg(0)?)))
        }
        // `{const, Term}` — a constant of any term form.
        "const" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Literal(args[0]))
        }
        // Comparison operators.
        "=:=" => pairwise(CmpOp::ExactEq),
        "=/=" => pairwise(CmpOp::ExactNe),
        "==" => pairwise(CmpOp::ArithEq),
        "/=" => pairwise(CmpOp::ArithNe),
        "<" => pairwise(CmpOp::Lt),
        "=<" => pairwise(CmpOp::Le),
        ">" => pairwise(CmpOp::Gt),
        ">=" => pairwise(CmpOp::Ge),
        // Type tests. `is_function/1` and `is_integer/1` also have a wider form
        // (`is_function/2`, `is_integer/3`) that the C `guard_tab` carries; the
        // other arities are `badarg` (verified against `erl`).
        "is_atom" => one(TypeTest::Atom),
        "is_integer" => match args.len() {
            1 => one(TypeTest::Integer),
            3 => Ok(Expr::IsInteger3(
                Box::new(arg(0)?),
                Box::new(arg(1)?),
                Box::new(arg(2)?),
            )),
            _ => Err(badarg()),
        },
        "is_float" => one(TypeTest::Float),
        "is_number" => one(TypeTest::Number),
        "is_tuple" => one(TypeTest::Tuple),
        "is_list" => one(TypeTest::List),
        "is_binary" => one(TypeTest::Binary),
        "is_bitstring" => one(TypeTest::Bitstring),
        "is_map" => one(TypeTest::Map),
        "is_pid" => one(TypeTest::Pid),
        "is_port" => one(TypeTest::Port),
        "is_reference" => one(TypeTest::Reference),
        "is_function" => match args.len() {
            1 => one(TypeTest::Function),
            2 => Ok(Expr::IsFunction2(Box::new(arg(0)?), Box::new(arg(1)?))),
            _ => Err(badarg()),
        },
        "is_boolean" => one(TypeTest::Boolean),
        // `is_record/3` only — the oracle's `is_record/2` is `badarg` (the C
        // `guard_tab` binds the three-argument form alone).
        "is_record" => match args.len() {
            3 => Ok(Expr::IsRecord3(
                Box::new(arg(0)?),
                Box::new(arg(1)?),
                Box::new(arg(2)?),
            )),
            _ => Err(badarg()),
        },
        // Guard BIFs.
        "element" => {
            if args.len() != 2 {
                return Err(badarg());
            }
            Ok(Expr::Element(Box::new(arg(0)?), Box::new(arg(1)?)))
        }
        "abs" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Abs(Box::new(arg(0)?)))
        }
        // Arithmetic. `+`/`-` are unary as well as binary — the C `guard_tab`
        // carries both arities — while the rest are binary only; any other arity
        // is `badarg` (verified against `erl`, which rejects `{'+',1,2,3}`,
        // `{'+'}`, `{'div',1}` and `{abs,1,2}` alike).
        "+" | "-" => {
            if args.is_empty() || args.len() > 2 {
                return Err(badarg());
            }
            let op = if name == "+" { ArithOp::Add } else { ArithOp::Sub };
            let mut parts = Vec::with_capacity(args.len());
            for index in 0..args.len() {
                parts.push(arg(index)?);
            }
            Ok(Expr::Arith(op, parts))
        }
        "*" => binary_arith(ArithOp::Mul),
        "/" => binary_arith(ArithOp::Div),
        "div" => binary_arith(ArithOp::IntDiv),
        "rem" => binary_arith(ArithOp::Rem),
        "band" => binary_arith(ArithOp::Band),
        "bor" => binary_arith(ArithOp::Bor),
        "bxor" => binary_arith(ArithOp::Bxor),
        "bsl" => binary_arith(ArithOp::Bsl),
        "bsr" => binary_arith(ArithOp::Bsr),
        "size" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Size(Box::new(arg(0)?)))
        }
        "hd" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Hd(Box::new(arg(0)?)))
        }
        "tl" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Tl(Box::new(arg(0)?)))
        }
        "length" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Length(Box::new(arg(0)?)))
        }
        // The `guard_tab` additions below are all in the C's callable-guard table
        // (`erl_db_util.c:572-976`) — each was probed against the system `erl` in
        // both a guard and a body.
        "map_size" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::MapSize(Box::new(arg(0)?)))
        }
        "map_get" => {
            if args.len() != 2 {
                return Err(badarg());
            }
            Ok(Expr::MapGet(Box::new(arg(0)?), Box::new(arg(1)?)))
        }
        "is_map_key" => {
            if args.len() != 2 {
                return Err(badarg());
            }
            Ok(Expr::IsMapKey(Box::new(arg(0)?), Box::new(arg(1)?)))
        }
        "tuple_size" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::TupleSize(Box::new(arg(0)?)))
        }
        "byte_size" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::ByteSize(Box::new(arg(0)?)))
        }
        "bit_size" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::BitSize(Box::new(arg(0)?)))
        }
        "float" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Float(Box::new(arg(0)?)))
        }
        "trunc" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Trunc(Box::new(arg(0)?)))
        }
        "round" => {
            if args.len() != 1 {
                return Err(badarg());
            }
            Ok(Expr::Round(Box::new(arg(0)?)))
        }
        "ceil" => unary(Expr::Ceil),
        "floor" => unary(Expr::Floor),
        "max" => binary(Expr::Max),
        "min" => binary(Expr::Min),
        // `binary_part/2` takes a **runtime** `{Pos, Len}` 2-tuple; `/3` takes the
        // two positions separately. Any other arity is `badarg`.
        "binary_part" => match args.len() {
            2 => Ok(Expr::BinaryPart2(Box::new(arg(0)?), Box::new(arg(1)?))),
            3 => Ok(Expr::BinaryPart3(
                Box::new(arg(0)?),
                Box::new(arg(1)?),
                Box::new(arg(2)?),
            )),
            _ => Err(badarg()),
        },
        "node" => match args.len() {
            0 => Ok(Expr::Node0),
            1 => Ok(Expr::Node1(Box::new(arg(0)?))),
            _ => Err(badarg()),
        },
        "xor" => binary(Expr::Xor),
        "bnot" => unary(Expr::Bnot),
        "self" => {
            if !args.is_empty() {
                return Err(badarg());
            }
            Ok(Expr::SelfPid)
        }
        _ => Err(badarg()),
    }
}

/// Evaluate an expression against a clause's bindings and the matched object.
///
/// Comparisons, type tests and the boolean connectives yield [`Value::Bool`] so
/// guard truth is independent of the caller's [`AtomTable`]; everything else
/// yields [`Value::Term`]. A term context (tuple/list template, `element/2`,
/// …) renders a boolean through [`Value::into_term`], which needs `true`/`false`
/// interned — inherent, since the result is the atom itself.
///
/// A guard caller treats `Err` as "the clause does not match"; a body caller
/// propagates it as `badarg` (mirroring `ets`: a raising guard skips the clause,
/// a raising body is an error).
pub(super) fn eval(
    expr: &Expr,
    binds: &[(u32, Eterm)],
    object: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Value> {
    match expr {
        Expr::Literal(term) => Ok(Value::Term(*term)),
        Expr::Bool(flag) => Ok(Value::Bool(*flag)),
        Expr::Bind(index) => binds
            .iter()
            .find(|(var, _)| var == index)
            .map(|(_, value)| Value::Term(*value))
            .ok_or_else(badarg),
        Expr::WholeObject => Ok(Value::Term(object)),
        Expr::AllBindings(vars) => {
            let mut acc = NIL;
            for index in vars.iter().rev() {
                let value = binds
                    .iter()
                    .find(|(var, _)| var == index)
                    .map(|(_, value)| *value)
                    .ok_or_else(badarg)?;
                acc = heap.cons(value, acc)?;
            }
            Ok(Value::Term(acc))
        }
        Expr::Tuple(parts) => {
            let mut values = Vec::with_capacity(parts.len());
            for part in parts {
                values.push(eval_term(part, binds, object, atoms, heap)?);
            }
            heap.tuple(&values).map(Value::Term)
        }
        Expr::Cons(head, tail) => {
            let head_value = eval_term(head, binds, object, atoms, heap)?;
            let tail_value = eval_term(tail, binds, object, atoms, heap)?;
            heap.cons(head_value, tail_value).map(Value::Term)
        }
        // `and`: strict — every operand is evaluated (and must be a boolean);
        // the result is their conjunction.
        Expr::And(parts) => {
            let mut result = true;
            for part in parts {
                if !value_bool(eval(part, binds, object, atoms, heap)?, atoms)? {
                    result = false;
                }
            }
            Ok(Value::Bool(result))
        }
        // `andalso`: short-circuit — the first `false` ends evaluation.
        Expr::AndAlso(parts) => {
            for part in parts {
                if !value_bool(eval(part, binds, object, atoms, heap)?, atoms)? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        // `or`: strict.
        Expr::Or(parts) => {
            let mut result = false;
            for part in parts {
                if value_bool(eval(part, binds, object, atoms, heap)?, atoms)? {
                    result = true;
                }
            }
            Ok(Value::Bool(result))
        }
        // `orelse`: short-circuit — the first `true` ends evaluation.
        Expr::OrElse(parts) => {
            for part in parts {
                if value_bool(eval(part, binds, object, atoms, heap)?, atoms)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        Expr::Not(part) => {
            let value = value_bool(eval(part, binds, object, atoms, heap)?, atoms)?;
            Ok(Value::Bool(!value))
        }
        Expr::Cmp(op, left, right) => {
            let left = eval_term(left, binds, object, atoms, heap)?;
            let right = eval_term(right, binds, object, atoms, heap)?;
            Ok(Value::Bool(guard::compare(*op, left, right, atoms)))
        }
        Expr::Test(test, operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            Ok(Value::Bool(guard::type_test(*test, operand, atoms)))
        }
        Expr::Element(position, tuple) => {
            let position = eval_term(position, binds, object, atoms, heap)?;
            let tuple = eval_term(tuple, binds, object, atoms, heap)?;
            guard::element(position, tuple).map(Value::Term)
        }
        Expr::Abs(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            match classify(operand) {
                // `abs(MIN_SMALL)` is a bignum in the oracle, so the widened value is
                // range-checked rather than handed to `make_small`.
                TermClass::Small => try_make_small(signed_value(operand).abs())
                    .map(Value::Term)
                    .map_err(|_| unsupported("match-spec 'abs': the result is not a small integer")),
                // SAFETY: the class established a live float block.
                TermClass::Float => {
                    let value = unsafe { float_value(operand) };
                    guard::write_float_term(heap, value.abs()).map(Value::Term)
                }
                // The absolute value of a bignum needs bignum arithmetic: a typed gap,
                // never a `badarg` that would silently under-match a guard.
                TermClass::Big => Err(unsupported("match-spec 'abs': a bignum operand")),
                // The oracle raises `badarg` for every other operand.
                _ => Err(badarg()),
            }
        }
        Expr::Arith(op, parts) => {
            let mut values = Vec::with_capacity(parts.len());
            for part in parts {
                values.push(eval_term(part, binds, object, atoms, heap)?);
            }
            // The hook is resolved through the thread-local seam and its `Rc` is
            // cloned out of the `RefCell` before the call (see [`arith_hook`]), so
            // no borrow is held while the hook runs.
            arith_hook().eval(*op, &values, atoms, heap).map(Value::Term)
        }
        Expr::Size(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::size(operand).map(Value::Term)
        }
        Expr::Hd(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            if !is_list(operand) {
                return Err(badarg());
            }
            // SAFETY: `is_list(operand)` established a live cons cell.
            Ok(Value::Term(unsafe { car(operand) }))
        }
        Expr::Tl(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            if !is_list(operand) {
                return Err(badarg());
            }
            // SAFETY: `is_list(operand)` established a live cons cell.
            Ok(Value::Term(unsafe { cdr(operand) }))
        }
        Expr::Length(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            let mut cursor = operand;
            let mut count: i64 = 0;
            while is_list(cursor) {
                // SAFETY: `is_list(cursor)` established a live cons cell.
                cursor = unsafe { cdr(cursor) };
                count += 1;
            }
            if !is_nil(cursor) {
                return Err(badarg());
            }
            Ok(Value::Term(make_small(count)))
        }
        Expr::MapSize(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            map_size(operand, atoms)
                .map(|count| Value::Term(make_small(count as i64)))
                .map_err(|_| badarg())
        }
        Expr::MapGet(key, map) => {
            let key = eval_term(key, binds, object, atoms, heap)?;
            let map = eval_term(map, binds, object, atoms, heap)?;
            map_get(map, key, atoms)
                .map_err(|_| badarg())?
                .map(Value::Term)
                .ok_or_else(badarg)
        }
        Expr::IsMapKey(key, map) => {
            let key = eval_term(key, binds, object, atoms, heap)?;
            let map = eval_term(map, binds, object, atoms, heap)?;
            let present = map_get(map, key, atoms).map_err(|_| badarg())?.is_some();
            Ok(Value::Bool(present))
        }
        Expr::TupleSize(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::tuple_size_of(operand).map(Value::Term)
        }
        Expr::ByteSize(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::bitstring_length(operand, true).map(Value::Term)
        }
        Expr::BitSize(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::bitstring_length(operand, false).map(Value::Term)
        }
        Expr::Float(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::float_of(operand, heap).map(Value::Term)
        }
        Expr::Trunc(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::integral_of(operand, false).map(Value::Term)
        }
        Expr::Round(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::integral_of(operand, true).map(Value::Term)
        }
        Expr::Map(pairs) => {
            let mut built = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                built.push((
                    eval_term(key, binds, object, atoms, heap)?,
                    eval_term(value, binds, object, atoms, heap)?,
                ));
            }
            guard::build_map(built, atoms, heap).map(Value::Term)
        }
        // `self/0` resolves through the [`SelfHook`] seam: without an installed
        // host hook the guard raises (the clause is then skipped). The default is
        // therefore a **semantic** under-match of a construct the ETS layer cannot
        // evaluate — see `guards_hold`'s classification.
        Expr::SelfPid => Ok(Value::Term(guard::self_term()?)),
        Expr::Ceil(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::ceil_floor(operand, true).map(Value::Term)
        }
        Expr::Floor(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::ceil_floor(operand, false).map(Value::Term)
        }
        Expr::Max(left, right) => {
            let left = eval_term(left, binds, object, atoms, heap)?;
            let right = eval_term(right, binds, object, atoms, heap)?;
            Ok(Value::Term(guard::max_min(left, right, atoms, true)))
        }
        Expr::Min(left, right) => {
            let left = eval_term(left, binds, object, atoms, heap)?;
            let right = eval_term(right, binds, object, atoms, heap)?;
            Ok(Value::Term(guard::max_min(left, right, atoms, false)))
        }
        Expr::BinaryPart2(binary, spec) => {
            let binary = eval_term(binary, binds, object, atoms, heap)?;
            let spec = eval_term(spec, binds, object, atoms, heap)?;
            guard::binary_part2(binary, spec, heap).map(Value::Term)
        }
        Expr::BinaryPart3(binary, position, length) => {
            let binary = eval_term(binary, binds, object, atoms, heap)?;
            let position = eval_term(position, binds, object, atoms, heap)?;
            let length = eval_term(length, binds, object, atoms, heap)?;
            guard::binary_part3(binary, position, length, heap).map(Value::Term)
        }
        Expr::IsRecord3(term, name, size) => {
            let term = eval_term(term, binds, object, atoms, heap)?;
            let name = eval_term(name, binds, object, atoms, heap)?;
            let size = eval_term(size, binds, object, atoms, heap)?;
            guard::is_record3(term, name, size, atoms).map(Value::Bool)
        }
        Expr::IsFunction2(term, arity) => {
            let term = eval_term(term, binds, object, atoms, heap)?;
            let arity = eval_term(arity, binds, object, atoms, heap)?;
            guard::is_function2(term, arity).map(Value::Bool)
        }
        Expr::IsInteger3(value, lower, upper) => {
            let value = eval_term(value, binds, object, atoms, heap)?;
            let lower = eval_term(lower, binds, object, atoms, heap)?;
            let upper = eval_term(upper, binds, object, atoms, heap)?;
            guard::is_integer3(value, lower, upper, atoms).map(Value::Bool)
        }
        Expr::Node0 => guard::node0(atoms).map(Value::Term),
        Expr::Node1(term) => {
            let term = eval_term(term, binds, object, atoms, heap)?;
            guard::node1(term, atoms).map(Value::Term)
        }
        Expr::Xor(left, right) => {
            let left = eval_term(left, binds, object, atoms, heap)?;
            let right = eval_term(right, binds, object, atoms, heap)?;
            guard::xor(left, right, atoms).map(Value::Bool)
        }
        Expr::Bnot(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            guard::bnot(operand).map(Value::Term)
        }
    }
}

/// Evaluate `expr` and require an ordinary term: a boolean result is rendered as
/// its `true`/`false` atom (which must be interned in `atoms`).
fn eval_term(
    expr: &Expr,
    binds: &[(u32, Eterm)],
    object: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Eterm> {
    eval(expr, binds, object, atoms, heap)?.into_term(atoms)
}

/// Interpret an expression value as a guard boolean: an internal boolean, or the
/// literal atoms `true`/`false`. Any other value (a non-boolean) is `badarg`, so
/// its clause fails — exactly as `erl` treats a non-boolean guard.
pub(super) fn value_bool(value: Value, atoms: &AtomTable) -> Result<bool> {
    match value {
        Value::Bool(flag) => Ok(flag),
        Value::Term(term) => guard::bool_of_atom(term, atoms).ok_or_else(badarg),
    }
}

/// The term class of a live term (a thin wrapper documenting the `unsafe`).
///
/// Shared with [`guard`] so every helper classifies a term through the same
/// documented probe.
pub(super) fn classify(term: Eterm) -> TermClass {
    // SAFETY: `class` reads a boxed header only for a boxed word; `term` is a live
    // term (a spec subterm or a matched value), so its header is valid.
    unsafe { class(term) }
}
