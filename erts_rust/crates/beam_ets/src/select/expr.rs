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

use std::collections::BTreeSet;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::{atom_name, var_index};
use crate::error::badarg;

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
    /// `size/1`.
    Size(Box<Expr>),
    /// `hd/1`.
    Hd(Box<Expr>),
    /// `tl/1`.
    Tl(Box<Expr>),
    /// `length/1`.
    Length(Box<Expr>),
    /// `self/0`.
    SelfPid,
    /// An internal boolean literal. Never produced by [`decode`] (a literal
    /// `true`/`false` decodes to [`Expr::Literal`] so it still renders as its
    /// atom); the pattern-only builders whose body is `[true]` (`match_delete`)
    /// use it so the deletion test does not depend on the `AtomTable`.
    Bool(bool),
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
            Value::Bool(flag) => bool_term(flag, atoms),
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
    // A map expression would have to be constructed (its values may be
    // variables); that is outside the supported subset.
    if classify(term) == TermClass::Map {
        return Err(badarg());
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
        // Type tests.
        "is_atom" => one(TypeTest::Atom),
        "is_integer" => one(TypeTest::Integer),
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
        "is_function" => one(TypeTest::Function),
        "is_boolean" => one(TypeTest::Boolean),
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
            Ok(Value::Bool(compare(*op, left, right, atoms)))
        }
        Expr::Test(test, operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            Ok(Value::Bool(type_test(*test, operand, atoms)))
        }
        Expr::Element(position, tuple) => {
            let position = eval_term(position, binds, object, atoms, heap)?;
            let tuple = eval_term(tuple, binds, object, atoms, heap)?;
            element(position, tuple).map(Value::Term)
        }
        Expr::Abs(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            if !is_small(operand) {
                return Err(badarg());
            }
            Ok(Value::Term(make_small(signed_value(operand).abs())))
        }
        Expr::Size(operand) => {
            let operand = eval_term(operand, binds, object, atoms, heap)?;
            size(operand).map(Value::Term)
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
        // No process identity at the ETS layer: the guard raises (the clause is
        // skipped) rather than producing a wrong pid.
        Expr::SelfPid => Err(badarg()),
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
        Value::Term(term) => bool_of_atom(term, atoms).ok_or_else(badarg),
    }
}

/// The term class of a live term (a thin wrapper documenting the `unsafe`).
fn classify(term: Eterm) -> TermClass {
    // SAFETY: `class` reads a boxed header only for a boxed word; `term` is a live
    // term (a spec subterm or a matched value), so its header is valid.
    unsafe { class(term) }
}

/// A comparison, using `beam_term::compare` (never a local term order).
fn compare(op: CmpOp, left: Eterm, right: Eterm, atoms: &AtomTable) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match op {
        CmpOp::ExactEq => cmp(left, right, atoms) == Equal,
        CmpOp::ExactNe => cmp(left, right, atoms) != Equal,
        CmpOp::ArithEq => cmp_arith(left, right, atoms) == Equal,
        CmpOp::ArithNe => cmp_arith(left, right, atoms) != Equal,
        CmpOp::Lt => cmp_arith(left, right, atoms) == Less,
        CmpOp::Le => cmp_arith(left, right, atoms) != Greater,
        CmpOp::Gt => cmp_arith(left, right, atoms) == Greater,
        CmpOp::Ge => cmp_arith(left, right, atoms) != Less,
    }
}

/// A type test (`is_*`).
fn type_test(test: TypeTest, operand: Eterm, atoms: &AtomTable) -> bool {
    use TermClass::*;
    let kind = classify(operand);
    match test {
        TypeTest::Atom => kind == Atom,
        TypeTest::Integer => matches!(kind, Small | Big),
        TypeTest::Float => kind == Float,
        TypeTest::Number => matches!(kind, Small | Big | Float),
        TypeTest::Tuple => kind == Tuple,
        // `is_list([])` is true in Erlang.
        TypeTest::List => matches!(kind, List | Nil),
        // SAFETY: `is_binary` reads the header of a bitstring term only; the class
        // check guarantees that.
        TypeTest::Binary => (kind == Bitstring && unsafe { is_binary(operand) }) || kind == BinRef,
        TypeTest::Bitstring => matches!(kind, Bitstring | BinRef),
        TypeTest::Map => kind == Map,
        TypeTest::Pid => matches!(kind, Pid | ExternalPid),
        TypeTest::Port => matches!(kind, Port | ExternalPort),
        TypeTest::Reference => matches!(kind, Ref | ExternalRef),
        TypeTest::Function => kind == Fun,
        TypeTest::Boolean => matches!(atom_name(operand, atoms), Some("true") | Some("false")),
    }
}

/// `element(N, Tuple)`.
fn element(position: Eterm, tuple: Eterm) -> Result<Eterm> {
    if !is_small(position) {
        return Err(badarg());
    }
    let index = signed_value(position);
    if index < 1 {
        return Err(badarg());
    }
    if !unsafe { is_tuple(tuple) } {
        return Err(badarg());
    }
    // SAFETY: `is_tuple(tuple)` established a live tuple header.
    let arity = unsafe { tuple_size(tuple) };
    let offset = usize::try_from(index - 1).map_err(|_| badarg())?;
    if offset >= arity {
        return Err(badarg());
    }
    // SAFETY: `offset < arity == tuple_size(tuple)`.
    Ok(unsafe { tuple_element(tuple, offset) })
}

/// `size/1`: the arity of a tuple, or the byte size of a binary.
fn size(operand: Eterm) -> Result<Eterm> {
    let kind = classify(operand);
    if kind == TermClass::Tuple {
        // SAFETY: `class` established a live tuple header.
        let arity = unsafe { tuple_size(operand) };
        return Ok(make_small(arity as i64));
    }
    // SAFETY: `is_binary` reads the header of a bitstring term only; the class
    // check guarantees that.
    if kind == TermClass::Bitstring && unsafe { is_binary(operand) } {
        // SAFETY: `is_binary` established a byte-aligned bitstring.
        let bits = unsafe { bitstring_size(operand) };
        return Ok(make_small((bits / 8) as i64));
    }
    Err(badarg())
}

/// The atom `true`/`false`.
fn bool_term(value: bool, atoms: &AtomTable) -> Result<Eterm> {
    let name = if value { "true" } else { "false" };
    atoms.lookup(name).map(make_atom).ok_or_else(badarg)
}

/// Interpret a term as a boolean: the literal atom `true`/`false`, else `None`.
fn bool_of_atom(term: Eterm, atoms: &AtomTable) -> Option<bool> {
    match atom_name(term, atoms) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}
