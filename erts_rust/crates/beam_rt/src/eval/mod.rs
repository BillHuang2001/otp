//! The `-eval` Erlang expression evaluator (contract A.6).
//!
//! `-eval Expr` in this build evaluates a **general Erlang expression** — an
//! `erl_eval`-style tree-walk over the [`parse`]d AST — so the observable result
//! matches the system `erl` for the expression surface: literals in every base,
//! lists/tuples/maps/binaries, the full operator set with Erlang precedence,
//! `case`/`if`/`begin`/`try`/`catch`/`receive`, `fun`s and comprehensions.
//!
//! # Semantics inherited from the lower crates (nothing is re-implemented)
//!
//! * **Arithmetic and comparison are BIFs.** Every `+ - * / div rem` and every
//!   `== /= =:= =/= < =< > >=` is dispatched through
//!   [`RuntimeContext::call_bif`](crate::context::RuntimeContext) to the
//!   `beam_bifs` `erlang` registry, so the results *and* the error reasons
//!   (`1/0` ⇒ `error:badarith`) are exactly the ones loaded code sees.
//! * **A call is a call.** A local `f(Args)` is an auto-imported
//!   `erlang:f/Arity`; `m:f(Args)` is a remote call. Both go through
//!   [`call_bif`](beam_engine::ExecutionContext::call_bif), which is also the
//!   load-on-demand path to the real stdlib (`lists:reverse/1` works without a
//!   `-pa`).
//! * **Terms are built through [`beam_engine::terms`].** `beam_rt` stays
//!   `#![deny(unsafe_code)]`; term layout (workspace `CONTEXT.md` §2) is never
//!   touched here.
//!
//! The handful of operators the `beam_bifs` surface deliberately does not own —
//! `and`/`or`/`xor`/`not`/`andalso`/`orelse` (Erlang *special forms* with
//! short-circuit semantics), `++`/`--` and the bitwise family — are implemented
//! here with the oracle's error behaviour (`not 5` ⇒ `error:badarg`).
//!
//! # Exceptions
//!
//! The evaluator carries its own control-flow signal ([`Flow`]) so that a raise
//! keeps its **live reason term**: `catch`/`try` must be able to hand a
//! structured reason (`{badmatch, Value}`) to a pattern, which the term-free
//! `beam_common::ExitReason` cannot express ([`beam_bifs::reason`] documents the
//! same gap). A [`BeamError`] returned by a BIF is converted once
//! ([`Evaluator::flow_from_error`]); `BeamError::Raise` keeps its original value
//! as the `origin`, so an uncaught exception reaches the CLI with exactly the
//! diagnostic the raising crate built.
//!
//! **`erlang:halt/0,1` and a capability gap are uncatchable.** The halt raise travels
//! as [`Flow::Fatal`], which no `catch`/`try` clause matches — that is what makes
//! `… halt(0) catch … end` terminate with status 0. A raise carrying the
//! `unsupported*` convention (a Milestone-1 capability gap: `unsupported_opcode: …`
//! from the engine, `unsupported_bif` from the BIF registry) is likewise [`Flow::Fatal`]
//! — a probe wrapped in `try … catch … end` must not turn a documented gap into an
//! apparent `exception`, so it propagates out and the CLI exits 3 with
//! `beam_rt: unsupported: <reason>` (see [`crate::error::unsupported_reason`]).
//!
//! # Known Milestone-1 gaps (typed errors, never wrong results)
//!
//! * `!` (send) — there is no mailbox in `-eval`, so it is a typed `unsupported`.
//! * `receive` without an `after` clause — likewise (it would block forever).
//! * `catch Expr` yields `{'EXIT', Reason}` for the `error`/`exit` classes (the
//!   oracle appends a stacktrace); M1 evaluates without one.
//!
//! A `fun` built here **can** escape into a loaded module: the closure bodies live on
//! the [`Runtime`](crate::runtime::Runtime) (not on the evaluator), so a higher-order
//! stdlib call such as `lists:map(fun(X) -> X*X end, …)` applies it back through
//! [`RuntimeContext::apply_fun`](crate::context::RuntimeContext).

mod ast;
mod parse;

use std::collections::HashMap;
use std::rc::Rc;

use beam_common::{BeamError, ExceptionClass, Raise, Result};
use beam_engine::{terms, ExecutionContext};
use beam_term::{
    atom_val, is_atom, is_list, is_nil, is_small, make_atom, make_small, signed_value,
    try_make_small, Eterm, NIL,
};

use crate::context::RuntimeContext;
use crate::error;

use ast::{is_var_name, BinOp, CatchClause, Clause, Expr, MapField, Qual, Seg, SegKind, Try, UnOp};

/// The `uniq` stamped into every fun this evaluator builds, so a fun can be
/// recognised as one of *its* closures and resolved through
/// [`Runtime::eval_closures`](crate::runtime::Runtime).
pub(crate) const EVAL_FUN_UNIQ: u64 = 0x6265_616d_7274_0001;

/// Converts a `Result<_, BeamError>` into the evaluator's control flow.
///
/// A term-building failure is not an Erlang exception — it is a typed runtime
/// error — so it travels as [`Flow::Fatal`] (uncatchable, reported by the CLI).
macro_rules! heap {
    ($e:expr) => {
        match $e {
            Ok(value) => value,
            Err(err) => return Err(Flow::Fatal(err)),
        }
    };
}

/// The evaluator's non-local control flow.
enum Flow {
    /// An Erlang exception with its live reason term.
    Raise {
        /// The exception class.
        class: ExceptionClass,
        /// The reason term (already built on the heap).
        reason: Eterm,
        /// The original error, when the raise came from a BIF (kept so an
        /// uncaught exception reaches the CLI unchanged).
        origin: Option<BeamError>,
    },
    /// An uncatchable error: a `halt`, or a typed runtime failure.
    Fatal(BeamError),
}

/// The evaluator's result type.
type EResult<T> = core::result::Result<T, Flow>;

/// The variable environment: names to terms (the task's `HashMap<String, Eterm>`).
type Env = HashMap<String, Eterm>;

/// The body of a fun built by this evaluator.
///
/// Stored on the [`Runtime`](crate::runtime::Runtime) (not on the evaluator) so a fun
/// that escapes into a loaded module — `lists:map(F, …)` calling `F` back — can still
/// be applied through [`RuntimeContext::apply_fun`](crate::context::RuntimeContext).
pub(crate) struct FunBody {
    /// What the fun does.
    kind: FunKind,
    /// The variables captured when the fun was created.
    env: Env,
}

/// The two kinds of fun this evaluator builds.
enum FunKind {
    /// `fun m:f/A` — apply `m:f` with the call's arguments.
    Remote {
        /// The target module.
        module: String,
        /// The target function.
        function: String,
        /// The target arity.
        arity: u32,
    },
    /// `fun (Params) -> Body end` — a closure over `env`.
    Local {
        /// The optional self-recursion name (`fun F(…) -> … end`).
        name: Option<String>,
        /// The clauses.
        clauses: Vec<ast::FunClause>,
    },
}

/// A comprehension template (what one iteration produces).
#[derive(Clone, Copy)]
enum CompTemplate<'x> {
    /// `[Template || …]`
    List(&'x Expr),
    /// `#{K => V || …}`
    Map(&'x Expr, &'x Expr),
    /// `<<Segs || …>>`
    Binary(&'x [Seg]),
}

/// The accumulator a comprehension fills.
enum CompOut {
    /// The list elements.
    List(Vec<Eterm>),
    /// The map pairs.
    Map(Vec<(Eterm, Eterm)>),
    /// The binary bytes.
    Binary(Vec<u8>),
}

/// Parses and evaluates `expr` against the runtime's BIF registry.
///
/// The result is built on the process heap, so it stays valid for the runtime's
/// lifetime.
///
/// # Errors
///
/// * a [`BeamError`] carrying a parse diagnostic ([`crate::error::bad_eval`]) for
///   anything outside the expression grammar — a capability gap, classified by
///   [`crate::error::unsupported_reason`];
/// * the error of whatever BIF/module the expression called, a raise of the
///   evaluated expression (including `erlang:halt/0,1`), or a documented
///   Milestone-1 gap ([`crate::error::unsupported`]).
pub fn eval(ctx: &mut RuntimeContext<'_>, expr: &str) -> Result<Eterm> {
    let tree = parse::parse(expr)?;
    let mut evaluator = Evaluator { ctx };
    let mut env = Env::new();
    let outcome = evaluator.expr(&tree, &mut env);
    finish(outcome, evaluator.ctx)
}

/// Converts the evaluator's control flow into a runtime [`Result`].
///
/// A [`Flow::Fatal`] is returned unchanged. A [`Flow::Raise`] that came from a BIF
/// (`origin`) is returned unchanged too; one the evaluator produced itself is
/// re-built as a [`BeamError::Raise`] from its live reason term.
fn finish(result: EResult<Eterm>, ctx: &RuntimeContext<'_>) -> Result<Eterm> {
    match result {
        Ok(value) => Ok(value),
        Err(Flow::Fatal(err)) => Err(err),
        Err(Flow::Raise {
            class,
            reason,
            origin,
        }) => match origin {
            Some(err) => Err(err),
            None => {
                let reason = beam_bifs::exit_reason_from_term(
                    reason,
                    ctx.runtime.code_server.atoms(),
                );
                Err(BeamError::Raise(Raise::new(class, reason)))
            }
        },
    }
}

/// Applies one of the `-eval` evaluator's closures by `index`, with `args`.
///
/// [`RuntimeContext::apply_fun`](crate::context::RuntimeContext) calls this when a
/// loaded module applies a fun whose `uniq` is [`EVAL_FUN_UNIQ`] (e.g.
/// `lists:map(fun(X) -> X*X end, …)`). A fresh [`Evaluator`] is built over the same
/// host so the closure body is evaluated and its errors are converted with exactly
/// the semantics a direct `-eval` application would use.
///
/// # Errors
///
/// [`BeamError::BadArg`] when no closure is stored at `index` (the fun is an `-eval`
/// fun, but not one this runtime built), plus whatever the closure raises.
pub(crate) fn apply_eval_closure(
    ctx: &mut RuntimeContext<'_>,
    fun: Eterm,
    index: usize,
    args: &[Eterm],
) -> Result<Eterm> {
    // Clone the `Rc` so the store is not borrowed while the fresh evaluator runs.
    let Some(body) = ctx.runtime.eval_closures.get(index).cloned() else {
        return Err(BeamError::BadArg);
    };
    let mut evaluator = Evaluator { ctx };
    let result = evaluator.apply_closure(&body, fun, args);
    finish(result, evaluator.ctx)
}

/// The tree-walking evaluator.
struct Evaluator<'a, 'b> {
    /// The host: the BIF registry, the shared atom table, the heap, the pid.
    ctx: &'a mut RuntimeContext<'b>,
}

impl Evaluator<'_, '_> {
    // -- expressions ----------------------------------------------------------

    /// Evaluates `expr` in `env`.
    fn expr(&mut self, expr: &Expr, env: &mut Env) -> EResult<Eterm> {
        match expr {
            Expr::Int(value) => self.small(*value),
            Expr::Float(value) => Ok(heap!(terms::make_float(self.ctx, *value))),
            Expr::Atom(name) => Ok(self.atom(name)),
            Expr::Str(text) => self.string(text),
            Expr::Nil => Ok(NIL),
            Expr::Var(name) => self.variable(name, env),

            Expr::List(elements, tail) => self.list(elements, tail.as_deref(), env),
            Expr::Tuple(elements) => self.tuple(elements, env),
            Expr::Map(fields) => self.map(fields, env),
            Expr::Binary(segments) => self.binary(segments, env),

            Expr::Unary(op, operand) => self.unary(*op, operand, env),
            Expr::Op(op, lhs, rhs) => self.operator(*op, lhs, rhs, env),

            Expr::Call(name, args) => self.local_call(name, args, env),
            Expr::Remote(module, function, args) => self.remote_call(module, function, args, env),
            Expr::Apply(callee, args) => self.apply_expr(callee, args, env),

            Expr::Case(subject, clauses) => self.case(subject, clauses, env),
            Expr::If(clauses) => self.if_expr(clauses, env),
            Expr::Block(body) => self.exprs(body, env),
            Expr::Try(try_block) => self.try_expr(try_block, env),
            Expr::Receive(receive) => self.receive(receive, env),

            Expr::FunRef {
                module,
                name,
                arity,
            } => self.fun_ref(module.as_deref(), name, *arity),
            Expr::Fun { name, clauses } => self.fun_literal(name.as_deref(), clauses, env),

            Expr::ListComp(template, quals) => {
                self.comprehension(CompTemplate::List(template), quals, env)
            }
            Expr::MapComp(key, value, quals) => {
                self.comprehension(CompTemplate::Map(key, value), quals, env)
            }
            Expr::BinComp(segments, quals) => {
                self.comprehension(CompTemplate::Binary(segments), quals, env)
            }

            Expr::Catch(inner) => self.catch_expr(inner, env),
        }
    }

    /// Evaluates a comma sequence, yielding the last value.
    fn exprs(&mut self, body: &[Expr], env: &mut Env) -> EResult<Eterm> {
        let mut last = NIL;
        for expression in body {
            last = self.expr(expression, env)?;
        }
        Ok(last)
    }

    /// Evaluates each expression to a term.
    fn terms(&mut self, exprs: &[Expr], env: &mut Env) -> EResult<Vec<Eterm>> {
        let mut values = Vec::with_capacity(exprs.len());
        for expression in exprs {
            values.push(self.expr(expression, env)?);
        }
        Ok(values)
    }

    /// `[E1, …, En | Tail]`.
    fn list(&mut self, elements: &[Expr], tail: Option<&Expr>, env: &mut Env) -> EResult<Eterm> {
        let mut acc = match tail {
            Some(tail) => self.expr(tail, env)?,
            None => NIL,
        };
        for element in elements.iter().rev() {
            let head = self.expr(element, env)?;
            acc = heap!(terms::make_cons(self.ctx, head, acc));
        }
        Ok(acc)
    }

    /// `{E1, …, En}`.
    fn tuple(&mut self, elements: &[Expr], env: &mut Env) -> EResult<Eterm> {
        let values = self.terms(elements, env)?;
        Ok(heap!(terms::make_tuple(self.ctx, &values)))
    }

    /// `#{K => V, …}` — later duplicates win, and `:=` requires the key to exist.
    fn map(&mut self, fields: &[MapField], env: &mut Env) -> EResult<Eterm> {
        let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(fields.len());
        for field in fields {
            let key = self.expr(&field.key, env)?;
            let value = self.expr(&field.value, env)?;
            let existing = pairs.iter().position(|(k, _)| self.exact_eq(*k, key));
            match existing {
                Some(index) => pairs[index].1 = value,
                None if field.update => return Err(self.raise(ExceptionClass::Error, "badarg")),
                None => pairs.push((key, value)),
            }
        }
        Ok(heap!(terms::make_flatmap(self.ctx, &pairs)))
    }

    /// `<<Seg, …>>`.
    fn binary(&mut self, segments: &[Seg], env: &mut Env) -> EResult<Eterm> {
        let mut bytes = Vec::new();
        for segment in segments {
            self.append_segment(segment, env, &mut bytes)?;
        }
        Ok(heap!(terms::make_heap_binary(self.ctx, &bytes)))
    }

    /// Appends one binary segment's bytes to `out`.
    fn append_segment(&mut self, segment: &Seg, env: &mut Env, out: &mut Vec<u8>) -> EResult<()> {
        let value = self.expr(&segment.value, env)?;
        match segment.kind {
            Some(SegKind::Utf8) => return self.push_utf8(value, out),
            Some(SegKind::Binary) | Some(SegKind::Bytes) | Some(SegKind::Bitstring)
            | Some(SegKind::Bits) => {
                let bytes = match terms::binary_bytes(value) {
                    Ok(bytes) => bytes,
                    Err(_) => return Err(self.raise(ExceptionClass::Error, "badarg")),
                };
                out.extend_from_slice(bytes);
                return Ok(());
            }
            Some(SegKind::Float) => {
                return Err(Flow::Fatal(error::unsupported("float binary segments")))
            }
            Some(SegKind::Utf16) | Some(SegKind::Utf32) => {
                return Err(Flow::Fatal(error::unsupported("utf16/utf32 binary segments")))
            }
            Some(SegKind::Integer) | None => {}
        }
        // A string segment: a list of byte-sized integers with no size/type.
        if segment.size.is_none() && segment.kind.is_none() {
            if let Ok(elements) = terms::list_elements(value) {
                for element in elements {
                    out.push(self.byte_of(element)?);
                }
                return Ok(());
            }
        }
        self.push_integer_segment(segment, value, env, out)
    }

    /// A numeric segment: `Value[:Size][/integer-…]`.
    fn push_integer_segment(
        &mut self,
        segment: &Seg,
        value: Eterm,
        env: &mut Env,
        out: &mut Vec<u8>,
    ) -> EResult<()> {
        if !is_small(value) {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        let number = signed_value(value);
        let mut bits: u32 = 8;
        if let Some(size) = &segment.size {
            let size = self.expr(size, env)?;
            if !is_small(size) {
                return Err(self.raise(ExceptionClass::Error, "badarg"));
            }
            let size = u32::try_from(signed_value(size))
                .map_err(|_| Flow::Fatal(error::unsupported("a negative segment size")))?;
            bits = match segment.unit {
                Some(unit) => size
                    .checked_mul(unit)
                    .ok_or_else(|| Flow::Fatal(error::unsupported("segment size overflow")))?,
                None => size,
            };
        }
        if bits == 0 || bits > 64 || bits % 8 != 0 {
            return Err(Flow::Fatal(error::unsupported(
                "a non-byte-aligned binary segment",
            )));
        }
        let bytes = (bits / 8) as usize;
        let signed = segment.signed.unwrap_or(false);
        let fits = if signed {
            let limit = 1i128 << (bits - 1);
            (i128::from(number) >= -limit) && (i128::from(number) < limit)
        } else {
            number >= 0 && (u128::from(number as u64) < (1u128 << bits))
        };
        if !fits {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        let mut buffer = match segment.endian {
            Some(ast::Endian::Little) => number.to_le_bytes().to_vec(),
            _ => number.to_be_bytes().to_vec(),
        };
        if matches!(segment.endian, Some(ast::Endian::Little)) {
            buffer.truncate(bytes);
        } else {
            buffer.drain(0..8 - bytes);
        }
        out.extend_from_slice(&buffer);
        Ok(())
    }

    /// Encodes `value` as UTF-8 and appends the bytes.
    fn push_utf8(&mut self, value: Eterm, out: &mut Vec<u8>) -> EResult<()> {
        if !is_small(value) {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        let code = u32::try_from(signed_value(value))
            .map_err(|_| self.raise(ExceptionClass::Error, "badarg"))?;
        let character = char::from_u32(code)
            .ok_or_else(|| self.raise(ExceptionClass::Error, "badarg"))?;
        let mut buffer = [0u8; 4];
        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        Ok(())
    }

    /// A byte-sized small integer, or `badarg`.
    fn byte_of(&mut self, value: Eterm) -> EResult<u8> {
        if !is_small(value) {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        u8::try_from(signed_value(value))
            .map_err(|_| self.raise(ExceptionClass::Error, "badarg"))
    }

    /// A prefix operator.
    fn unary(&mut self, op: UnOp, operand: &Expr, env: &mut Env) -> EResult<Eterm> {
        let value = self.expr(operand, env)?;
        match op {
            UnOp::Plus => self.call("erlang", "+", &[value]),
            UnOp::Minus => self.call("erlang", "-", &[value]),
            UnOp::Not => {
                let boolean = self.boolean(value)?;
                Ok(self.bool_term(!boolean))
            }
            UnOp::Bnot => {
                let integer = self.integer(value)?;
                self.small(!integer)
            }
        }
    }

    /// An infix operator.
    fn operator(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, env: &mut Env) -> EResult<Eterm> {
        match op {
            BinOp::Match => self.match_op(lhs, rhs, env),
            BinOp::AndAlso => {
                let left = self.expr(lhs, env)?;
                if self.boolean(left)? {
                    self.expr(rhs, env)
                } else {
                    Ok(self.bool_term(false))
                }
            }
            BinOp::OrElse => {
                let left = self.expr(lhs, env)?;
                if self.boolean(left)? {
                    Ok(self.bool_term(true))
                } else {
                    self.expr(rhs, env)
                }
            }
            BinOp::And | BinOp::Or | BinOp::Xor => {
                let left_value = self.expr(lhs, env)?;
                let left = self.boolean(left_value)?;
                let right_value = self.expr(rhs, env)?;
                let right = self.boolean(right_value)?;
                let result = match op {
                    BinOp::And => left && right,
                    BinOp::Or => left || right,
                    _ => left != right,
                };
                Ok(self.bool_term(result))
            }
            BinOp::ListConcat => self.list_concat(lhs, rhs, env),
            BinOp::ListSubtract => self.list_subtract(lhs, rhs, env),
            BinOp::Band | BinOp::Bor | BinOp::Bxor | BinOp::Bsl | BinOp::Bsr => {
                let left_value = self.expr(lhs, env)?;
                let left = self.integer(left_value)?;
                let right_value = self.expr(rhs, env)?;
                let right = self.integer(right_value)?;
                let result = match op {
                    BinOp::Band => left & right,
                    BinOp::Bor => left | right,
                    BinOp::Bxor => left ^ right,
                    BinOp::Bsl => {
                        let shift = u32::try_from(right).ok().filter(|s| *s < 64);
                        match shift.and_then(|shift| left.checked_shl(shift)) {
                            Some(result) => result,
                            None => return Err(self.raise(ExceptionClass::Error, "system_limit")),
                        }
                    }
                    _ => {
                        let shift = u32::try_from(right).ok().filter(|s| *s < 64);
                        match shift {
                            Some(shift) => left >> shift,
                            None => return Err(self.raise(ExceptionClass::Error, "badarith")),
                        }
                    }
                };
                self.small(result)
            }
            BinOp::Send => Err(Flow::Fatal(error::unsupported(
                "the send operator '!' (no mailbox in -eval)",
            ))),
            _ => {
                let left = self.expr(lhs, env)?;
                let right = self.expr(rhs, env)?;
                let Some(name) = operator_bif(op) else {
                    return Err(Flow::Fatal(error::unsupported("this operator")));
                };
                self.call("erlang", name, &[left, right])
            }
        }
    }

    /// `Pattern = Expr`.
    fn match_op(&mut self, pattern: &Expr, rhs: &Expr, env: &mut Env) -> EResult<Eterm> {
        let value = self.expr(rhs, env)?;
        if self.match_pattern(pattern, value, env)? {
            return Ok(value);
        }
        let tag = self.atom("badmatch");
        let reason = heap!(terms::make_tuple(self.ctx, &[tag, value]));
        Err(Flow::Raise {
            class: ExceptionClass::Error,
            reason,
            origin: None,
        })
    }

    /// `A ++ B`.
    fn list_concat(&mut self, lhs: &Expr, rhs: &Expr, env: &mut Env) -> EResult<Eterm> {
        let left = self.expr(lhs, env)?;
        let right = self.expr(rhs, env)?;
        let Ok(elements) = terms::list_elements(left) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        let mut result = right;
        for element in elements.into_iter().rev() {
            result = heap!(terms::make_cons(self.ctx, element, result));
        }
        Ok(result)
    }

    /// `A -- B`.
    fn list_subtract(&mut self, lhs: &Expr, rhs: &Expr, env: &mut Env) -> EResult<Eterm> {
        let left = self.expr(lhs, env)?;
        let right = self.expr(rhs, env)?;
        let Ok(mut elements) = terms::list_elements(left) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        let Ok(removed) = terms::list_elements(right) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        for candidate in &removed {
            if let Some(index) = elements.iter().position(|e| self.exact_eq(*e, *candidate)) {
                elements.remove(index);
            }
        }
        Ok(heap!(terms::make_list(self.ctx, &elements)))
    }

    /// A local call: `f(Args)` is `erlang:f/Arity`; `F(Args)` applies a fun.
    fn local_call(&mut self, name: &str, args: &[Expr], env: &mut Env) -> EResult<Eterm> {
        if is_var_name(name) {
            let function = self.variable(name, env)?;
            let values = self.terms(args, env)?;
            return self.apply_value(function, &values);
        }
        self.call_function("erlang", name, args, env)
    }

    /// A remote call `m:f(Args)`.
    fn remote_call(
        &mut self,
        module: &str,
        function: &str,
        args: &[Expr],
        env: &mut Env,
    ) -> EResult<Eterm> {
        self.call_function(module, function, args, env)
    }

    /// Applies the value of `callee` (a fun term) to the evaluated `args`.
    fn apply_expr(&mut self, callee: &Expr, args: &[Expr], env: &mut Env) -> EResult<Eterm> {
        let function = self.expr(callee, env)?;
        let values = self.terms(args, env)?;
        self.apply_value(function, &values)
    }

    /// Dispatches `module:function(Args)`.
    fn call_function(
        &mut self,
        module: &str,
        function: &str,
        arg_exprs: &[Expr],
        env: &mut Env,
    ) -> EResult<Eterm> {
        let args = self.terms(arg_exprs, env)?;
        let arity = args.len() as u32;
        // The raise helpers carry the *live* reason term; the atom-only
        // `ExitReason` model would lose a structured reason, so they are served
        // here (the semantics are exactly `erlang:error/1,2`, `exit/1`,
        // `throw/1`, `raise/3`).
        if module == "erlang" {
            match (function, arity) {
                ("error", 1) | ("error", 2) => {
                    return Err(Flow::Raise {
                        class: ExceptionClass::Error,
                        reason: args[0],
                        origin: None,
                    })
                }
                ("exit", 1) => {
                    return Err(Flow::Raise {
                        class: ExceptionClass::Exit,
                        reason: args[0],
                        origin: None,
                    })
                }
                ("throw", 1) => {
                    return Err(Flow::Raise {
                        class: ExceptionClass::Throw,
                        reason: args[0],
                        origin: None,
                    })
                }
                ("raise", 3) => return self.raise_3(&args),
                ("nif_error", 1) | ("nif_error", 2) => {
                    // `erlang:nif_error/1,2` raises `error` with its **first argument**
                    // as the reason (the second is crash-report metadata, per
                    // `nif_error_2` in `../../erts/emulator/beam/bif.c:1525`). A
                    // compound reason is served here rather than by the registered BIF:
                    // `beam_common::ExitReason` cannot carry a term, so the BIF's
                    // structured reason would lose it. Keeping the live term preserves
                    // the exact reason (`{nif_not_loaded,m,f,0}` stays that tuple).
                    return Err(Flow::Raise {
                        class: ExceptionClass::Error,
                        reason: args[0],
                        origin: None,
                    });
                }
                ("apply", 2) => return self.apply_2(&args),
                ("apply", 3) => return self.apply_3(&args),
                _ => {}
            }
        }
        self.call(module, function, &args)
    }

    /// `erlang:raise(Class, Reason, Stacktrace)`.
    fn raise_3(&mut self, args: &[Eterm]) -> EResult<Eterm> {
        let class = match self.atom_name(args[0]).as_deref() {
            Some("error") => ExceptionClass::Error,
            Some("exit") => ExceptionClass::Exit,
            Some("throw") => ExceptionClass::Throw,
            _ => return Err(self.raise(ExceptionClass::Error, "badarg")),
        };
        Err(Flow::Raise {
            class,
            reason: args[1],
            origin: None,
        })
    }

    /// `erlang:apply(Fun, Args)`.
    fn apply_2(&mut self, args: &[Eterm]) -> EResult<Eterm> {
        let Ok(call_args) = terms::list_elements(args[1]) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        self.apply_value(args[0], &call_args)
    }

    /// `erlang:apply(Module, Function, Args)`.
    fn apply_3(&mut self, args: &[Eterm]) -> EResult<Eterm> {
        let (Some(module), Some(function)) = (self.atom_name(args[0]), self.atom_name(args[1]))
        else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        let Ok(call_args) = terms::list_elements(args[2]) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        self.call(&module, &function, &call_args)
    }

    /// Applies a fun value: one of this evaluator's closures, or a `.beam` fun.
    fn apply_value(&mut self, function: Eterm, args: &[Eterm]) -> EResult<Eterm> {
        if terms::is_fun(function) {
            let uniq = heap!(terms::fun_uniq(function));
            let index = heap!(terms::fun_index(function)) as usize;
            if uniq == EVAL_FUN_UNIQ {
                if let Some(body) = self.ctx.runtime.eval_closures.get(index).cloned() {
                    return self.apply_closure(&body, function, args);
                }
            }
        }
        match self.ctx.apply_fun(function, args) {
            Ok(value) => Ok(value),
            Err(err) => Err(self.flow_from_error(err)),
        }
    }

    /// Applies one of this evaluator's closures.
    fn apply_closure(
        &mut self,
        body: &Rc<FunBody>,
        function: Eterm,
        args: &[Eterm],
    ) -> EResult<Eterm> {
        match &body.kind {
            FunKind::Remote {
                module,
                function: name,
                arity,
            } => {
                if args.len() != *arity as usize {
                    return Err(Flow::Fatal(BeamError::BadArity));
                }
                self.call(module, name, args)
            }
            FunKind::Local { name, clauses } => {
                for clause in clauses {
                    if clause.params.len() != args.len() {
                        continue;
                    }
                    let mut clause_env = body.env.clone();
                    if let Some(name) = name {
                        clause_env.insert(name.clone(), function);
                    }
                    let mut matched = true;
                    for (param, arg) in clause.params.iter().zip(args) {
                        match self.match_pattern(param, *arg, &mut clause_env) {
                            Ok(true) => {}
                            Ok(false) => {
                                matched = false;
                                break;
                            }
                            Err(flow) => return Err(flow),
                        }
                    }
                    if !matched {
                        continue;
                    }
                    if !self.guards_ok(&clause.guards, &mut clause_env)? {
                        continue;
                    }
                    return self.exprs(&clause.body, &mut clause_env);
                }
                Err(Flow::Fatal(BeamError::BadArity))
            }
        }
    }

    /// `case Subject of Clauses end`.
    fn case(&mut self, subject: &Expr, clauses: &[Clause], env: &mut Env) -> EResult<Eterm> {
        let value = self.expr(subject, env)?;
        if let Some(result) = self.match_clauses(clauses, value, env)? {
            return Ok(result);
        }
        let tag = self.atom("case_clause");
        let reason = heap!(terms::make_tuple(self.ctx, &[tag, value]));
        Err(Flow::Raise {
            class: ExceptionClass::Error,
            reason,
            origin: None,
        })
    }

    /// Matches `value` against `clauses`, evaluating the first matching clause.
    fn match_clauses(
        &mut self,
        clauses: &[Clause],
        value: Eterm,
        env: &Env,
    ) -> EResult<Option<Eterm>> {
        for clause in clauses {
            let mut clause_env = env.clone();
            if let Some(pattern) = &clause.pattern {
                if !self.match_pattern(pattern, value, &mut clause_env)? {
                    continue;
                }
            }
            if !self.guards_ok(&clause.guards, &mut clause_env)? {
                continue;
            }
            return Ok(Some(self.exprs(&clause.body, &mut clause_env)?));
        }
        Ok(None)
    }

    /// `if Clauses end`.
    fn if_expr(&mut self, clauses: &[Clause], env: &mut Env) -> EResult<Eterm> {
        for clause in clauses {
            let mut clause_env = env.clone();
            if !self.guards_ok(&clause.guards, &mut clause_env)? {
                continue;
            }
            return self.exprs(&clause.body, &mut clause_env);
        }
        Err(self.raise(ExceptionClass::Error, "if_clause"))
    }

    /// `try … end`.
    fn try_expr(&mut self, try_block: &Try, env: &mut Env) -> EResult<Eterm> {
        let mut body_env = env.clone();
        let outcome = match self.exprs(&try_block.body, &mut body_env) {
            Ok(value) => {
                if try_block.of_clauses.is_empty() {
                    Ok(value)
                } else {
                    match self.match_clauses(&try_block.of_clauses, value, env)? {
                        Some(result) => Ok(result),
                        None => {
                            let tag = self.atom("try_clause");
                            let reason = heap!(terms::make_tuple(self.ctx, &[tag, value]));
                            Err(Flow::Raise {
                                class: ExceptionClass::Error,
                                reason,
                                origin: None,
                            })
                        }
                    }
                }
            }
            Err(Flow::Fatal(err)) => Err(Flow::Fatal(err)),
            Err(Flow::Raise {
                class, reason, ..
            }) => self.catch_clauses(&try_block.catches, class, reason, env),
        };
        if !try_block.after.is_empty() {
            let mut after_env = env.clone();
            self.exprs(&try_block.after, &mut after_env)?;
        }
        outcome
    }

    /// Matches an exception against a `try`'s catch clauses.
    fn catch_clauses(
        &mut self,
        clauses: &[CatchClause],
        class: ExceptionClass,
        reason: Eterm,
        env: &Env,
    ) -> EResult<Eterm> {
        let class_term = self.atom(class.as_str());
        for clause in clauses {
            let mut clause_env = env.clone();
            let mut matched = true;
            if let Some(pattern) = &clause.class {
                matched = self.match_pattern(pattern, class_term, &mut clause_env)?;
            }
            if matched {
                matched = self.match_pattern(&clause.reason, reason, &mut clause_env)?;
            }
            if matched {
                if let Some(pattern) = &clause.stacktrace {
                    matched = self.match_pattern(pattern, NIL, &mut clause_env)?;
                }
            }
            if !matched {
                continue;
            }
            if !self.guards_ok(&clause.guards, &mut clause_env)? {
                continue;
            }
            return self.exprs(&clause.body, &mut clause_env);
        }
        Err(Flow::Raise {
            class,
            reason,
            origin: None,
        })
    }

    /// `receive … end` — best effort: there is no mailbox in `-eval`.
    fn receive(&mut self, receive: &ast::Receive, env: &mut Env) -> EResult<Eterm> {
        match &receive.after {
            Some((timeout, body)) => {
                let mut timeout_env = env.clone();
                self.expr(timeout, &mut timeout_env)?;
                let mut body_env = env.clone();
                self.exprs(body, &mut body_env)
            }
            None => Err(Flow::Fatal(error::unsupported(
                "receive without an after clause (no mailbox in -eval)",
            ))),
        }
    }

    /// `catch Expr`.
    fn catch_expr(&mut self, inner: &Expr, env: &mut Env) -> EResult<Eterm> {
        match self.expr(inner, env) {
            Ok(value) => Ok(value),
            Err(Flow::Fatal(err)) => Err(Flow::Fatal(err)),
            Err(Flow::Raise { class, reason, .. }) => {
                if class == ExceptionClass::Throw {
                    return Ok(reason);
                }
                let tag = self.atom("EXIT");
                Ok(heap!(terms::make_tuple(self.ctx, &[tag, reason])))
            }
        }
    }

    /// `fun Name/Arity` / `fun Mod:Name/Arity` — a local `fun f/A` is `erlang:f/A`.
    fn fun_ref(&mut self, module: Option<&str>, name: &str, arity: u32) -> EResult<Eterm> {
        let body = FunBody {
            kind: FunKind::Remote {
                module: module.unwrap_or("erlang").to_string(),
                function: name.to_string(),
                arity,
            },
            env: Env::new(),
        };
        self.register_fun(body, arity)
    }

    /// `fun [(Name)](Params) -> Body … end`.
    fn fun_literal(
        &mut self,
        name: Option<&str>,
        clauses: &[ast::FunClause],
        env: &mut Env,
    ) -> EResult<Eterm> {
        let arity = clauses.first().map_or(0, |clause| clause.params.len()) as u32;
        let body = FunBody {
            kind: FunKind::Local {
                name: name.map(str::to_string),
                clauses: clauses.to_vec(),
            },
            env: env.clone(),
        };
        self.register_fun(body, arity)
    }

    /// Builds a fun term for `body` and records the closure in the runtime's store.
    fn register_fun(&mut self, body: FunBody, arity: u32) -> EResult<Eterm> {
        let index = u32::try_from(self.ctx.runtime.eval_closures.len())
            .map_err(|_| Flow::Fatal(BeamError::SystemLimit))?;
        self.ctx.runtime.eval_closures.push(Rc::new(body));
        Ok(heap!(terms::make_fun(
            self.ctx,
            arity,
            EVAL_FUN_UNIQ,
            index,
            &[]
        )))
    }

    /// `[Template || Quals]`, `#{K => V || …}` and `<<Segs || …>>`.
    fn comprehension(
        &mut self,
        template: CompTemplate<'_>,
        quals: &[Qual],
        env: &mut Env,
    ) -> EResult<Eterm> {
        let mut out = match template {
            CompTemplate::List(_) => CompOut::List(Vec::new()),
            CompTemplate::Map(..) => CompOut::Map(Vec::new()),
            CompTemplate::Binary(_) => CompOut::Binary(Vec::new()),
        };
        self.comp_collect(&template, quals, 0, env, &mut out)?;
        match out {
            CompOut::List(elements) => Ok(heap!(terms::make_list(self.ctx, &elements))),
            CompOut::Map(pairs) => Ok(heap!(terms::make_flatmap(self.ctx, &pairs))),
            CompOut::Binary(bytes) => Ok(heap!(terms::make_heap_binary(self.ctx, &bytes))),
        }
    }

    /// Fills `out` with every combination the remaining qualifiers admit.
    fn comp_collect(
        &mut self,
        template: &CompTemplate<'_>,
        quals: &[Qual],
        index: usize,
        env: &mut Env,
        out: &mut CompOut,
    ) -> EResult<()> {
        if index >= quals.len() {
            return match template {
                CompTemplate::List(expression) => {
                    let value = self.expr(expression, env)?;
                    if let CompOut::List(elements) = out {
                        elements.push(value);
                    }
                    Ok(())
                }
                CompTemplate::Map(key, value) => {
                    let key = self.expr(key, env)?;
                    let value = self.expr(value, env)?;
                    if let CompOut::Map(pairs) = out {
                        pairs.push((key, value));
                    }
                    Ok(())
                }
                CompTemplate::Binary(segments) => {
                    let mut bytes = Vec::new();
                    for segment in segments.iter() {
                        self.append_segment(segment, env, &mut bytes)?;
                    }
                    if let CompOut::Binary(buffer) = out {
                        buffer.extend_from_slice(&bytes);
                    }
                    Ok(())
                }
            };
        }
        match &quals[index] {
            Qual::Filter(filter) => match self.expr(filter, env) {
                Ok(value) => {
                    if self.is_true(value) {
                        self.comp_collect(template, quals, index + 1, env, out)
                    } else {
                        Ok(())
                    }
                }
                Err(Flow::Fatal(err)) => Err(Flow::Fatal(err)),
                Err(Flow::Raise { .. }) => Ok(()),
            },
            Qual::Generator { pattern, source } => {
                let source = self.expr(source, env)?;
                let elements = self.iterable(source)?;
                for element in elements {
                    let mut branch = env.clone();
                    if self.match_pattern(pattern, element, &mut branch)? {
                        self.comp_collect(template, quals, index + 1, &mut branch, out)?;
                    }
                }
                Ok(())
            }
            Qual::BinaryGenerator { .. } => {
                Err(Flow::Fatal(error::unsupported("a binary generator")))
            }
        }
    }

    /// The elements a comprehension generator iterates.
    fn iterable(&mut self, value: Eterm) -> EResult<Vec<Eterm>> {
        if is_nil(value) {
            return Ok(Vec::new());
        }
        if let Ok(elements) = terms::list_elements(value) {
            return Ok(elements);
        }
        if let Ok(bytes) = terms::binary_bytes(value) {
            return Ok(bytes.iter().map(|b| make_small(i64::from(*b))).collect());
        }
        Err(self.raise(ExceptionClass::Error, "badarg"))
    }

    // -- patterns -------------------------------------------------------------

    /// Matches `pattern` against `value`, binding variables in `env`.
    ///
    /// A failure is `Ok(false)` — a clause that does not match, never an
    /// exception (`=` is the only construct that turns a failure into
    /// `error:{badmatch, V}`). The `Err` case is a *typed* failure (a pattern
    /// form this Milestone does not implement), never a wrong match.
    fn match_pattern(&mut self, pattern: &Expr, value: Eterm, env: &mut Env) -> EResult<bool> {
        match pattern {
            Expr::Var(name) => {
                if name == "_" {
                    return Ok(true);
                }
                match env.get(name).copied() {
                    Some(bound) => Ok(self.exact_eq(bound, value)),
                    None => {
                        env.insert(name.clone(), value);
                        Ok(true)
                    }
                }
            }
            Expr::List(elements, tail) => self.match_list(elements, tail.as_deref(), value, env),
            Expr::Tuple(elements) => self.match_tuple(elements, value, env),
            Expr::Map(fields) => self.match_map(fields, value, env),
            Expr::Binary(segments) => self.match_binary(segments, value, env),
            Expr::Op(BinOp::Match, left, right) => Ok(self.match_pattern(left, value, env)?
                && self.match_pattern(right, value, env)?),
            other => {
                let expected = self.expr(other, env)?;
                Ok(self.exact_eq(expected, value))
            }
        }
    }

    /// Matches `[P1, …, Pn | Tail]` against a (possibly improper) list.
    fn match_list(
        &mut self,
        patterns: &[Expr],
        tail: Option<&Expr>,
        mut rest: Eterm,
        env: &mut Env,
    ) -> EResult<bool> {
        for pattern in patterns {
            if !is_list(rest) {
                return Ok(false);
            }
            let head = heap!(terms::cons_head(rest));
            let next = heap!(terms::cons_tail(rest));
            if !self.match_pattern(pattern, head, env)? {
                return Ok(false);
            }
            rest = next;
        }
        match tail {
            Some(pattern) => self.match_pattern(pattern, rest, env),
            None => Ok(is_nil(rest)),
        }
    }

    /// Matches `{P1, …, Pn}`.
    fn match_tuple(&mut self, patterns: &[Expr], value: Eterm, env: &mut Env) -> EResult<bool> {
        let Ok(arity) = terms::tuple_arity(value) else {
            return Ok(false);
        };
        if arity != patterns.len() {
            return Ok(false);
        }
        for (index, pattern) in patterns.iter().enumerate() {
            let element = heap!(terms::tuple_element(value, index));
            if !self.match_pattern(pattern, element, env)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Matches `#{K := V, …}` (every key must be present).
    fn match_map(&mut self, fields: &[MapField], value: Eterm, env: &mut Env) -> EResult<bool> {
        let Ok(entries) = terms::map_entries(value) else {
            return Ok(false);
        };
        for field in fields {
            let key = self.expr(&field.key, env)?;
            let entry = entries
                .iter()
                .find(|(candidate, _)| self.exact_eq(*candidate, key))
                .map(|(_, entry)| *entry);
            let Some(entry) = entry else { return Ok(false) };
            if !self.match_pattern(&field.value, entry, env)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Matches a `<<…>>` pattern (byte-aligned segments only).
    fn match_binary(&mut self, segments: &[Seg], value: Eterm, env: &mut Env) -> EResult<bool> {
        let Ok(bytes) = terms::binary_bytes(value) else {
            return Ok(false);
        };
        let mut offset = 0usize;
        for (index, segment) in segments.iter().enumerate() {
            if segment.size.is_some() || segment.unit.is_some() {
                return Err(Flow::Fatal(error::unsupported(
                    "a sized binary pattern segment",
                )));
            }
            match segment.kind {
                None | Some(SegKind::Integer) => {
                    if offset >= bytes.len() {
                        return Ok(false);
                    }
                    let byte = make_small(i64::from(bytes[offset]));
                    offset += 1;
                    if !self.match_pattern(&segment.value, byte, env)? {
                        return Ok(false);
                    }
                }
                Some(SegKind::Binary) | Some(SegKind::Bytes) | Some(SegKind::Bitstring)
                | Some(SegKind::Bits) => {
                    if index + 1 != segments.len() {
                        return Err(Flow::Fatal(error::unsupported(
                            "a non-final binary pattern segment",
                        )));
                    }
                    let rest = heap!(terms::make_heap_binary(self.ctx, &bytes[offset..]));
                    offset = bytes.len();
                    if !self.match_pattern(&segment.value, rest, env)? {
                        return Ok(false);
                    }
                }
                _ => {
                    return Err(Flow::Fatal(error::unsupported(
                        "this binary pattern segment type",
                    )))
                }
            }
        }
        Ok(offset == bytes.len())
    }

    /// Evaluates guards: outer alternatives are `;` (or), inner are `,` (and).
    ///
    /// A guard that is false, that raises, or that yields a non-boolean makes its
    /// alternative fail (Erlang's rule); a `halt` still propagates.
    fn guards_ok(&mut self, guards: &[Vec<Expr>], env: &mut Env) -> EResult<bool> {
        if guards.is_empty() {
            return Ok(true);
        }
        'alternatives: for alternative in guards {
            let mut alternative_env = env.clone();
            for guard in alternative {
                match self.expr(guard, &mut alternative_env) {
                    Ok(value) => {
                        if !self.is_true(value) {
                            continue 'alternatives;
                        }
                    }
                    Err(Flow::Fatal(err)) => return Err(Flow::Fatal(err)),
                    Err(Flow::Raise { .. }) => continue 'alternatives,
                }
            }
            *env = alternative_env;
            return Ok(true);
        }
        Ok(false)
    }

    // -- primitives -----------------------------------------------------------

    /// Dispatches `module:function(args)` through the host, converting an error.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> EResult<Eterm> {
        match self
            .ctx
            .call_bif(module, function, args.len() as u32, args)
        {
            Ok(value) => Ok(value),
            Err(err) => Err(self.flow_from_error(err)),
        }
    }

    /// Converts a [`BeamError`] into the evaluator's control flow.
    ///
    /// A `halt` is [`Flow::Fatal`] — an Erlang `catch`/`try` must not intercept
    /// it. A **capability gap** is also fatal: a raise whose reason atom starts with
    /// `unsupported` (the `unsupported_opcode:`/`unsupported_bif` convention, read
    /// through the single classifier [`crate::error::unsupported_reason`]) is the
    /// runtime telling the host "this build cannot do that", not an Erlang
    /// exception — so a `try`/`catch` in the evaluated expression must not swallow
    /// it and the CLI can still exit 3 with `beam_rt: unsupported: <reason>`. Every
    /// genuine `error`/`exit`/`throw` stays fully catchable. A structured reason
    /// (which `beam_common` cannot carry as a term) is likewise fatal rather than
    /// silently reshaped.
    fn flow_from_error(&mut self, err: BeamError) -> Flow {
        if error::halt_status(&err).is_some() {
            return Flow::Fatal(err);
        }
        if error::unsupported_reason(&err).is_some() {
            return Flow::Fatal(err);
        }
        let converted = match &err {
            BeamError::Raise(raise) => raise
                .reason
                .as_atom()
                .map(|name| (raise.class, name.to_string())),
            BeamError::BadArg => Some((ExceptionClass::Error, "badarg".to_string())),
            BeamError::BadArith => Some((ExceptionClass::Error, "badarith".to_string())),
            BeamError::BadArity => Some((ExceptionClass::Error, "badarity".to_string())),
            BeamError::BadMatch => Some((ExceptionClass::Error, "badmatch".to_string())),
            BeamError::SystemLimit => Some((ExceptionClass::Error, "system_limit".to_string())),
            BeamError::Alloc(_) => Some((ExceptionClass::Error, "out_of_memory".to_string())),
            _ => None,
        };
        match converted {
            Some((class, name)) => {
                let reason = self.atom(&name);
                Flow::Raise {
                    class,
                    reason,
                    origin: Some(err),
                }
            }
            None => Flow::Fatal(err),
        }
    }

    /// A `system_limit`-checked small integer.
    fn small(&mut self, value: i64) -> EResult<Eterm> {
        match try_make_small(value) {
            Ok(term) => Ok(term),
            Err(_) => Err(self.raise(ExceptionClass::Error, "system_limit")),
        }
    }

    /// A string literal as an Erlang string (a list of character codes).
    fn string(&mut self, text: &str) -> EResult<Eterm> {
        let elements: Vec<Eterm> = text
            .chars()
            .map(|c| make_small(i64::from(u32::from(c))))
            .collect();
        Ok(heap!(terms::make_list(self.ctx, &elements)))
    }

    /// Reads a variable from `env`.
    fn variable(&mut self, name: &str, env: &Env) -> EResult<Eterm> {
        if name == "_" {
            return Err(Flow::Fatal(error::message(
                "the anonymous variable '_' has no value",
            )));
        }
        env.get(name).copied().ok_or_else(|| {
            Flow::Fatal(error::message(format!("variable {name:?} is unbound")))
        })
    }

    /// Builds an exception with an atom reason.
    fn raise(&mut self, class: ExceptionClass, name: &str) -> Flow {
        let reason = self.atom(name);
        Flow::Raise {
            class,
            reason,
            origin: None,
        }
    }

    /// `value` as a boolean, or `error:badarg`.
    fn boolean(&mut self, value: Eterm) -> EResult<bool> {
        match self.atom_name(value).as_deref() {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(self.raise(ExceptionClass::Error, "badarg")),
        }
    }

    /// `value` as a small integer, or `error:badarg`.
    fn integer(&mut self, value: Eterm) -> EResult<i64> {
        if is_small(value) {
            Ok(signed_value(value))
        } else {
            Err(self.raise(ExceptionClass::Error, "badarg"))
        }
    }

    /// Whether `value` is the atom `true` (a guard that yields anything else
    /// fails rather than raising).
    fn is_true(&self, value: Eterm) -> bool {
        self.atom_name(value).as_deref() == Some("true")
    }

    /// The atom term for `flag`.
    fn bool_term(&mut self, flag: bool) -> Eterm {
        if flag {
            self.atom("true")
        } else {
            self.atom("false")
        }
    }

    /// The exact (`=:=`) equality of two terms.
    fn exact_eq(&self, a: Eterm, b: Eterm) -> bool {
        beam_term::eq(a, b, self.ctx.runtime.code_server.atoms())
    }

    /// The name of the atom `value`, resolved through the runtime's shared table.
    fn atom_name(&self, value: Eterm) -> Option<String> {
        if !is_atom(value) {
            return None;
        }
        self.ctx
            .runtime
            .code_server
            .atoms()
            .name(atom_val(value))
            .map(str::to_string)
    }

    /// The atom `name`, interned in the runtime's shared table.
    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.ctx.runtime.code_server.atoms_mut().intern(name))
    }
}

/// The `erlang` BIF implementing a delegatable binary operator.
fn operator_bif(op: BinOp) -> Option<&'static str> {
    Some(match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::FloatDiv => "/",
        BinOp::IntDiv => "div",
        BinOp::Rem => "rem",
        BinOp::Eq => "==",
        BinOp::NotEq => "/=",
        BinOp::ExactEq => "=:=",
        BinOp::ExactNotEq => "=/=",
        BinOp::Lt => "<",
        BinOp::LtEq => "=<",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use beam_term::{atom_val, is_small, signed_value};

    use super::*;
    use crate::config::RuntimeConfig;
    use crate::runtime::Runtime;

    /// Evaluates `expr` (with no program output) and returns the term.
    ///
    /// The runtime (and its heap) is dropped when this returns, so the result is
    /// only usable while it is a **fixed immediate** (a small integer or an atom).
    /// A heap-allocated result (a float, list, tuple, binary, …) would dangle;
    /// such tests drive `RuntimeContext` directly so the runtime outlives the term.
    fn run(expr: &str) -> Result<Eterm> {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        eval(&mut ctx, expr)
    }

    /// Evaluates `expr` and asserts the result is the small integer `expected`.
    fn int(expr: &str, expected: i64) {
        let value = run(expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        assert!(is_small(value), "{expr} is not a small integer");
        assert_eq!(signed_value(value), expected, "{expr}");
    }

    /// Evaluates `expr` and asserts the result is the atom `expected`.
    fn atom(expr: &str, expected: &str) {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        assert_eq!(ctx.runtime.atoms().name(atom_val(value)), Some(expected));
    }

    #[test]
    fn arithmetic_and_precedence() {
        int("1+2", 3);
        int("2*3+4", 10);
        int("(1+2)*3", 9);
        int("10 - 3 - 2", 5);
        int("7 div 2", 3);
        int("7 rem 2", 1);
        int("-3 + 1", -2);
        int("2 * -3", -6);
    }

    #[test]
    fn integer_division_is_a_float_but_the_probe_only_needs_the_bif() {
        // `2/3` is `0.666…` — a float, exactly as `erl` returns it. The result is
        // heap-allocated, so the runtime must outlive it (see `run`).
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, "2/3").expect("2/3");
        assert!(!is_small(value));
        assert!((terms::float_value(value).expect("a float") - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn comparison_and_boolean_operators() {
        atom("1 == 1.0", "true");
        atom("1 =:= 1.0", "false");
        atom("hello < world", "true");
        atom("[1,2,3] < [1,2,4]", "true");
        atom("true andalso false", "false");
        atom("false orelse true", "true");
        atom("not false", "true");
        atom("true xor true", "false");
        // `++` builds a list (`[1,2] ++ [3]` is `[1,2,3]`), so assert the list, not a scalar.
        let concatenated = run("[1,2] ++ [3]").expect("[1,2] ++ [3]");
        let elements = terms::list_elements(concatenated).expect("a list");
        assert_eq!(elements.len(), 3);
        assert_eq!(elements[2], make_small(3));
    }

    #[test]
    fn andalso_returns_the_second_value() {
        // `true andalso 5` is `5` in the oracle.
        int("true andalso 5", 5);
        int("false orelse 5", 5);
    }

    #[test]
    fn non_booleans_raise_badarg() {
        for expr in ["not 5", "1 and true", "1 andalso true", "1 orelse true"] {
            let error = run(expr).expect_err(expr);
            assert!(
                matches!(&error, BeamError::Raise(raise) if raise.reason.as_atom() == Some("badarg")),
                "{expr}: {error}"
            );
        }
    }

    #[test]
    fn case_if_and_pattern_matching() {
        int("case 2 of 1 -> 10; 2 -> 20; _ -> 30 end", 20);
        int("if 1 > 2 -> 1; true -> 2 end", 2);
        int("X = 7, X + 1", 8);
        int("{A, B} = {1, 2}, A * 10 + B", 12);
        int("[H | T] = [1, 2, 3], H + length(T)", 3);
        int("#{a := V} = #{a => 5}, V", 5);
        // A failed match raises `{badmatch, V}`. The term-free `ExitReason` model
        // flags the structured reason (it cannot carry the tuple), so assert the
        // `error` class — the expression can only fail by badmatch.
        let error = run("X = 1, X = 2").expect_err("badmatch");
        assert!(
            matches!(&error, BeamError::Raise(raise) if raise.class == ExceptionClass::Error),
            "{error}"
        );
    }

    #[test]
    fn guards_and_clause_fallthrough() {
        // Clause 2 matches (`3 > 1`) and yields the atom `mid`.
        atom(
            "case 3 of N when N > 5 -> big; N when N > 1 -> mid; _ -> small end",
            "mid",
        );
        int(
            "case hello of 1 -> a; {X} -> b; Y when is_atom(Y) -> 99; _ -> d end",
            99,
        );
    }

    #[test]
    fn exceptions_are_caught_by_class_and_reason() {
        int("try 1/0 catch error:badarith -> 1 end", 1);
        int("try throw(x) catch throw:y -> 0; throw:x -> 1 end", 1);
        int("catch 42", 42);
        atom("catch throw(mine)", "mine");
        int("try erlang:error(boom) of V -> V catch error:R -> 7 end", 7);
        // `after` runs and overrides.
        int("try 1 catch _:_ -> 2 after 3 end", 1);
    }

    #[test]
    fn funs_are_created_applied_and_captured() {
        int("(fun(X) -> X + 1 end)(41)", 42);
        int("F = fun(X) -> X * 2 end, F(21)", 42);
        int("Apply = fun(F, A) -> F(A) end, Apply(fun(X) -> X + 1 end, 41)", 42);
        int("Add = fun F(0, Acc) -> Acc; F(N, Acc) -> F(N - 1, Acc + N) end, Add(4, 0)", 10);
    }

    #[test]
    fn comprehensions() {
        int("length([X * 2 || X <- [1,2,3]])", 3);
        int("lists:sum([X || X <- lists:seq(1, 5), X > 2])", 12);
    }

    #[test]
    fn system_info_through_the_native_floor() {
        atom("is_list(erlang:system_info(otp_release))", "true");
        atom("is_integer(erlang:system_info(process_count))", "true");
        int("length(erlang:system_info(version))", 4);
    }

    #[test]
    fn halt_is_uncatchable() {
        let error = run("try halt(7) catch _:_ -> 0 end").expect_err("halt terminates");
        assert_eq!(error::halt_status(&error), Some(7));
    }

    #[test]
    fn a_capability_gap_is_uncatchable() {
        // `erlang:send/2` has no delivery hook in `-eval`, so it raises the
        // `unsupported_bif` capability gap. A `try`/`catch` in the expression must
        // NOT turn that into an apparent exception: it propagates out (classified by
        // `unsupported_reason`) so the CLI exits 3 with `beam_rt: unsupported: …`.
        for expr in [
            "try erlang:send(self(), hello) catch _:_ -> 0 end",
            "try (erlang:send(self(), hello)) of V -> V catch C:R -> ok end",
        ] {
            let error = run(expr).expect_err("a capability gap propagates");
            assert!(
                error::unsupported_reason(&error).is_some(),
                "{expr}: {error}"
            );
        }
    }

    #[test]
    fn genuine_raises_stay_catchable_alongside_the_uncatchable_gap() {
        // The `unsupported*` convention is fatal, but a reason that merely *contains*
        // `unsupported` — or any other genuine raise — is an ordinary, catchable error.
        atom(
            "try erlang:error(unsupported_thing) catch error:R -> R end",
            "unsupported_thing",
        );
        int("try 1/0 catch error:R when R =:= badarith -> 9 end", 9);
        int("try throw(x) catch throw:x -> 11 end", 11);
    }

    #[test]
    fn a_loaded_module_applies_an_eval_closure() {
        // `lists:map/2` (a loaded stdlib module) calls the `-eval` fun back through
        // `RuntimeContext::apply_fun`; the runtime must resolve it from its closure
        // store. The result is heap-allocated, so the runtime must outlive it.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let mapped = eval(&mut ctx, "lists:map(fun(X) -> X*X end, [1,2,3])")
            .expect("lists:map/2 applies the eval fun");
        let elements = terms::list_elements(mapped).expect("a list");
        assert_eq!(elements.len(), 3);
        assert_eq!(signed_value(elements[0]), 1);
        assert_eq!(signed_value(elements[1]), 4);
        assert_eq!(signed_value(elements[2]), 9);
    }

    #[test]
    fn nif_error_raises_its_live_reason_term() {
        // `erlang:nif_error/1,2` raises `error` with its argument as the reason. The
        // `beam_bifs` reason is `Structured` and carries no term, so the evaluator
        // raises the **live argument term** itself: an atom reason stays an atom and a
        // compound reason survives verbatim (the differential `nif/nif_error_*` cases).
        atom("try erlang:nif_error(foo) catch error:R -> R end", "foo");

        // The tuple is heap-allocated, so the runtime must outlive it — drive the
        // `RuntimeContext` directly (see `run`) and print the reason.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let reason = eval(
            &mut ctx,
            "try erlang:nif_error({nif_not_loaded,m,f,0}) catch error:R -> R end",
        )
        .expect("the compound reason survives the raise");
        let printed = ctx.runtime.format_term(reason, None).expect("~p renders it");
        assert_eq!(printed.trim_end(), "{nif_not_loaded,m,f,0}");
    }

    #[test]
    fn malformed_expressions_are_capability_gaps() {
        for expr in ["1 +", "(", "case"] {
            let error = run(expr).expect_err(expr);
            assert!(error::unsupported_reason(&error).is_some(), "{expr}: {error}");
        }
    }

    #[test]
    fn documented_gaps_are_typed_not_wrong() {
        for expr in ["self() ! hello", "receive X -> X end"] {
            let error = run(expr).expect_err(expr);
            assert!(
                error::unsupported_reason(&error).is_some(),
                "{expr}: {error}"
            );
        }
    }
}
