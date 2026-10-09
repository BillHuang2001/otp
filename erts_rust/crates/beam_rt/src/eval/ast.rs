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

//! The abstract syntax tree of the `-eval` Erlang-expression grammar.
//!
//! The tree is deliberately *untyped*: an [`Expr`] is used both for expressions
//! and for patterns. Pattern positions are re-interpreted by the evaluator
//! ([`super`]'s `match_pattern`), exactly as `erl_eval` re-interprets the very
//! same `erl_parse` trees it evaluates.
//!
//! The grammar covered is the *expression* subset of Erlang: literals (integers
//! in any base, floats, atoms, strings, chars, lists, tuples, maps, binaries),
//! variables and `Pattern = Expr` matching, the full operator set with Erlang
//! precedence/associativity, `case`/`if`/`begin`/`try`/`receive`, `fun`
//! (anonymous, named and captured) and list/map/binary comprehensions. Module
//! forms (attributes, function definitions, records) are out of scope: `-eval`
//! takes an expression, not a module.

/// A binary operator with Erlang semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` (always float, `badarith` on a zero divisor)
    FloatDiv,
    /// `div` (integers only)
    IntDiv,
    /// `rem` (integers only)
    Rem,
    /// `==`
    Eq,
    /// `/=`
    NotEq,
    /// `=:=`
    ExactEq,
    /// `=/=`
    ExactNotEq,
    /// `<`
    Lt,
    /// `=<`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `and` (both operands evaluated)
    And,
    /// `or`
    Or,
    /// `xor`
    Xor,
    /// `andalso` (short-circuit)
    AndAlso,
    /// `orelse` (short-circuit)
    OrElse,
    /// `++`
    ListConcat,
    /// `--`
    ListSubtract,
    /// `band`
    Band,
    /// `bor`
    Bor,
    /// `bxor`
    Bxor,
    /// `bsl`
    Bsl,
    /// `bsr`
    Bsr,
    /// `=` (pattern match)
    Match,
    /// `!` (message send)
    Send,
}

/// A unary (prefix) operator with Erlang semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// unary `+`
    Plus,
    /// unary `-`
    Minus,
    /// `not`
    Not,
    /// `bnot`
    Bnot,
}

/// The endianness of a binary segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    /// `big`
    Big,
    /// `little`
    Little,
    /// `native`
    Native,
}

/// The type of a binary segment (Erlang's segment `Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    /// `integer`
    Integer,
    /// `float`
    Float,
    /// `binary`
    Binary,
    /// `bytes`
    Bytes,
    /// `bitstring`
    Bitstring,
    /// `bits`
    Bits,
    /// `utf8`
    Utf8,
    /// `utf16`
    Utf16,
    /// `utf32`
    Utf32,
}

/// One `<<…>>` segment: `Value [:Size] [/Type-…]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Seg {
    /// The segment's value expression.
    pub value: Expr,
    /// The explicit `:Size` (in bits, unless a `unit:` is given).
    pub size: Option<Expr>,
    /// The `Type` specifier, if any.
    pub kind: Option<SegKind>,
    /// The `signed`/`unsigned` specifier, if any.
    pub signed: Option<bool>,
    /// The endianness specifier, if any.
    pub endian: Option<Endian>,
    /// The `unit:N` specifier, if any.
    pub unit: Option<u32>,
}

/// A `case`/`if`/`receive`/`fun` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    /// The head pattern; `None` for an `if` guard clause.
    pub pattern: Option<Expr>,
    /// The guard sequence: outer = `;` (or), inner = `,` (and).
    pub guards: Vec<Vec<Expr>>,
    /// The clause body (a comma sequence of expressions).
    pub body: Vec<Expr>,
}

/// A `try`/`catch` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    /// The `Class:` pattern, if the `Class:Reason` form was used.
    pub class: Option<Expr>,
    /// The `Reason` pattern.
    pub reason: Expr,
    /// The optional `:Stacktrace` pattern.
    pub stacktrace: Option<Expr>,
    /// The guard sequence.
    pub guards: Vec<Vec<Expr>>,
    /// The clause body.
    pub body: Vec<Expr>,
}

/// One clause of a `fun` (anonymous or named).
#[derive(Debug, Clone, PartialEq)]
pub struct FunClause {
    /// The parameter patterns.
    pub params: Vec<Expr>,
    /// The guard sequence.
    pub guards: Vec<Vec<Expr>>,
    /// The clause body.
    pub body: Vec<Expr>,
}

/// A `try … [of …] [catch …] [after …] end` expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Try {
    /// The protected body.
    pub body: Vec<Expr>,
    /// The `of` clauses (matched against a successful body value).
    pub of_clauses: Vec<Clause>,
    /// The `catch` clauses.
    pub catches: Vec<CatchClause>,
    /// The `after` body (always run, its value discarded).
    pub after: Vec<Expr>,
}

/// A `receive … [after T -> …] end` expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Receive {
    /// The receive clauses.
    pub clauses: Vec<Clause>,
    /// The optional `after Timeout -> Body` (timeout expression, body).
    pub after: Option<(Expr, Vec<Expr>)>,
}

/// One comprehension qualifier.
#[derive(Debug, Clone, PartialEq)]
pub enum Qual {
    /// `Pattern <- Expr` (a list/binary generator).
    Generator {
        /// The element pattern.
        pattern: Expr,
        /// The source expression.
        source: Expr,
    },
    /// `Pattern <= Expr` (a binary generator).
    BinaryGenerator {
        /// The element pattern.
        pattern: Expr,
        /// The source expression.
        source: Expr,
    },
    /// A filter expression (kept when it is `true`).
    Filter(Expr),
}

/// One `#{Key => Value}` field (also used in map patterns).
#[derive(Debug, Clone, PartialEq)]
pub struct MapField {
    /// The key expression.
    pub key: Expr,
    /// The value expression.
    pub value: Expr,
    /// Whether the `:=` (update/require-present) form was used.
    pub update: bool,
}

/// An Erlang expression (also used as a pattern).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// An integer literal.
    ///
    /// `i128` so a literal past the 64-bit range still has an exact value (the evaluator
    /// materializes a bignum term from it); a literal past `i128` is a typed capability gap.
    Int(i128),
    /// A float literal.
    Float(f64),
    /// An atom (bare or quoted).
    Atom(String),
    /// A string literal (a list of character codes).
    Str(String),
    /// The empty list `[]`.
    Nil,
    /// A variable (`X`, `_`, `_Name`).
    Var(String),

    /// `[E1, …, En | Tail]` (`Tail` is `None` for a proper list).
    List(Vec<Expr>, Option<Box<Expr>>),
    /// `{E1, …, En}`
    Tuple(Vec<Expr>),
    /// `#{K => V, …}`
    Map(Vec<MapField>),
    /// `<<Seg, …>>`
    Binary(Vec<Seg>),

    /// A prefix operator application.
    Unary(UnOp, Box<Expr>),
    /// An infix operator application.
    ///
    /// Named `Op` (not `Binary`) so it cannot be confused with [`Expr::Binary`],
    /// the `<<…>>` literal.
    Op(BinOp, Box<Expr>, Box<Expr>),

    /// A local (auto-imported) call `f(Args)`, or `F(Args)` on a fun-valued
    /// variable when `name` starts with an upper-case letter or `_`.
    Call(String, Vec<Expr>),
    /// A remote call `Mod:Fun(Args)`.
    Remote(String, String, Vec<Expr>),
    /// A call whose callee is an arbitrary expression (a fun-valued term), e.g.
    /// `(fun(X) -> X end)(1)`.
    Apply(Box<Expr>, Vec<Expr>),

    /// `case Subject of Clauses end`
    Case(Box<Expr>, Vec<Clause>),
    /// `if Clauses end`
    If(Vec<Clause>),
    /// A comma sequence of expressions (a parenthesised `(E1, E2)` or `begin … end`).
    Block(Vec<Expr>),
    /// `try … end`
    Try(Box<Try>),
    /// `receive … end`
    Receive(Box<Receive>),

    /// `fun Name/Arity` / `fun Mod:Name/Arity`.
    FunRef {
        /// The module (`None` for a local `fun f/A`, which resolves to `erlang`).
        module: Option<String>,
        /// The function name.
        name: String,
        /// The arity.
        arity: u32,
    },
    /// `fun [(Name)](Params) -> Body … end`
    Fun {
        /// The (optional) self-recursion name.
        name: Option<String>,
        /// The clauses.
        clauses: Vec<FunClause>,
    },

    /// `[Template || Quals]`
    ListComp(Box<Expr>, Vec<Qual>),
    /// `#{K => V || Quals}`
    MapComp(Box<Expr>, Box<Expr>, Vec<Qual>),
    /// `<<Segs || Quals>>`
    BinComp(Vec<Seg>, Vec<Qual>),

    /// `catch Expr`
    Catch(Box<Expr>),
}

/// Whether `name` is written as an Erlang *variable* (upper-case or `_` start).
pub fn is_var_name(name: &str) -> bool {
    matches!(name.chars().next(), Some(c) if c.is_ascii_uppercase() || c == '_')
}
