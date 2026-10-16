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
//! * **Arithmetic, comparison and the bit operators are BIFs.** Every
//!   `+ - * / div rem` and every `== /= =:= =/= < =< > >=` is dispatched through
//!   [`RuntimeContext::call_bif`](crate::context::RuntimeContext) to the
//!   `beam_bifs` `erlang` registry, so the results *and* the error reasons
//!   (`1/0` ⇒ `error:badarith`) are exactly the ones loaded code sees. The same
//!   registry owns `band`/`bor`/`bxor`/`bnot`/`bsl`/`bsr`, so those six bit
//!   operators inherit its arbitrary-precision (bignum) semantics for free.
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
//! short-circuit semantics) and `++`/`--` — are implemented here with the
//! oracle's error behaviour (`not 5` ⇒ `error:badarg`).
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
//! * `receive` without an `after` clause **and** with nothing runnable to deliver a message
//!   is a typed `unsupported` (there is no host timer to block on); a bare `receive` inside a
//!   spawned process is too, because that context runs *inside* a scheduler slice and must
//!   not re-enter the scheduler.
//! * `catch Expr` folds **by class**, exactly like `erl_eval`: a `throw` yields the
//!   reason, an `exit` yields `{'EXIT', Reason}`, and an `error` yields
//!   `{'EXIT', {Reason, Stacktrace}}`. The trace is always a list (never dropped): it is
//!   the raise's synthesized **callee head item** — `{TargetModule, TargetFunction,
//!   ArgList, []}`, built at the failing call site of an `undef` — followed by any
//!   supplied `erlang:raise/3` frames.
//!   The residual limitation: a raise with neither (`erlang:error/1`, an arithmetic
//!   error) renders an **empty** trace, because the `-eval` tree-walker gathers no
//!   `erl_eval`/`init` frame of its own (a fabricated frame would be a wrong term).
//!
//! A `fun` built here **can** escape into a loaded module: the closure bodies live on
//! the [`Runtime`](crate::runtime::Runtime) (not on the evaluator), so a higher-order
//! stdlib call such as `lists:map(fun(X) -> X*X end, …)` applies it back through
//! [`RuntimeContext::apply_fun`](crate::context::RuntimeContext). A `fun m:f/A` is not a
//! closure at all: it is the real `EXPORT_EXT` term, applied through the host's dispatch.

mod ast;
mod bits;
mod bits_decode;
mod bits_match;
mod parse;

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_common::{BeamError, ExceptionClass, LiveReason, Raise, Result};
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

/// How many scheduler slices one `receive` pump may run before it gives up.
///
/// The pump exists so a process `erlang:spawn/1` started can run and deliver a message the
/// evaluator is waiting for. It must be **bounded**: `-eval` has no scheduler of its own and
/// a receive that could pump forever would hang the CLI (the very thing a capability gap is
/// meant to avoid). 64 slices is far more than the corpus needs and small enough that a
/// pathological program reports the gap promptly.
const RECEIVE_PUMP_SLICES: u64 = 64;

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
        /// The **synthesized innermost trace item** of an `undef` raised by an
        /// unresolved call target — `{CalleeModule, CalleeFunction, ArgList, []}`,
        /// built at the failing call site from that site's own callee identity and
        /// its **real** arguments (see [`Evaluator::with_callee_head_item`]).
        ///
        /// `None` for every other raise: the tree-walker renders no `erl_eval`
        /// frame of its own (there is no `erl_eval` port here), so this is the one
        /// item it can honestly contribute to a trace.
        head: Option<Eterm>,
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

/// The kind of fun this evaluator builds.
///
/// A `fun m:f/A` is **not** one of these: it is the real
/// [`FunKind::External`](beam_codec::FunKind::External) term (see
/// [`Evaluator::fun_ref`]), so only the local closure needs a stored body.
enum FunKind {
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
    /// The binary comprehension's accumulated bits.
    Bits(BitBuf),
}

/// A binary-construction buffer: the accumulated bits plus their count.
///
/// The bits are packed most-significant-bit first, exactly as Erlang's `<<…>>`
/// construction lays them out, so a segment need not be byte-aligned. `bytes`
/// always holds `ceil(bits / 8)` bytes, and the low bits of the last byte are left
/// clear when the total is not a whole number of bytes (`make_bitstring` zeroes them
/// too).
#[derive(Default)]
struct BitBuf {
    /// The accumulated bits, MSB-first within each byte.
    bytes: Vec<u8>,
    /// The number of bits written (`bytes.len() * 8` minus any trailing padding).
    bits: usize,
}

impl BitBuf {
    /// Appends one bit.
    fn push_bit(&mut self, bit: bool) {
        let index = self.bits / 8;
        if self.bits % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            self.bytes[index] |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    /// Appends the low `n_bits` bits of `value`, most significant first.
    fn push_bits(&mut self, value: u64, n_bits: usize) {
        for bit in (0..n_bits).rev() {
            self.push_bit(value >> bit & 1 == 1);
        }
    }

    /// Appends the low `n_bits` bits of the two's-complement integer whose magnitude is
    /// `magnitude` (`ErtsDigit`s, least significant first) and whose sign is `negative`.
    ///
    /// `n_bits` may exceed 64: the magnitude is widened to `ceil(n_bits / 64)` words
    /// (zero-padded) and a negative value is complemented within that width, exactly as
    /// Erlang truncates the value to `n_bits` in two's complement (`<<-1:2048>>` is 2048
    /// one-bits, `<<300:8>>` is `<<44>>`, `<<0:2048>>` is 256 zero bytes).
    ///
    /// A **big-endian** segment (`little == false`) emits the words most significant
    /// first, the top word truncated to its `n_bits % 64` bits. A **little-endian** segment
    /// is laid out **byte**-wise ([`bits::integer_le_image`]): the value's whole bytes follow
    /// least significant first, and a width that is not a whole number of bytes leaves the
    /// low `n_bits % 8` bits of the next byte as the final partial field, in the high bits of
    /// the last byte — `<<255:12/little>>` is `<<255,0:4>>` and `<<-300:12/little>>` is
    /// `<<212,14:4>>`, exactly the bit stream the oracle emits (and the one a matching
    /// `little` integer segment reads back).
    fn push_integer_bits(
        &mut self,
        magnitude: &[u64],
        negative: bool,
        n_bits: usize,
        little: bool,
    ) {
        let words_count = n_bits.div_ceil(64);
        let mut words = vec![0u64; words_count];
        let copied = magnitude.len().min(words_count);
        words[..copied].copy_from_slice(&magnitude[..copied]);
        if negative {
            // Two's complement within `words_count` words: invert every word, then add one,
            // propagating the carry. (`words_count >= 1` because `n_bits > 0` here.)
            let mut carry = true;
            for word in words.iter_mut() {
                let (value, overflow) = (!*word).overflowing_add(u64::from(carry));
                *word = value;
                carry = overflow;
            }
        }
        let top_bits = n_bits - 64 * (words_count - 1);
        if little {
            let (bytes, tail, rem) = bits::integer_le_image(&words, n_bits);
            for byte in bytes {
                self.push_byte(byte);
            }
            if rem != 0 {
                self.push_bits(tail, rem);
            }
        } else {
            self.push_bits(words[words_count - 1] & mask_bits(top_bits), top_bits);
            for index in (0..words_count - 1).rev() {
                self.push_bits(words[index], 64);
            }
        }
    }

    /// Appends one byte (eight bits, most significant first).
    fn push_byte(&mut self, byte: u8) {
        self.push_bits(u64::from(byte), 8);
    }

    /// Appends `n_bits` bits of `bytes` starting at bit `start` (MSB-first within
    /// each byte).
    fn push_bit_slice(&mut self, bytes: &[u8], start: usize, n_bits: usize) {
        // Fast path: a byte-aligned slice onto a byte-aligned buffer is a copy.
        if self.bits % 8 == 0 && start % 8 == 0 && n_bits % 8 == 0 {
            let first = start / 8;
            self.bytes.extend_from_slice(&bytes[first..first + n_bits / 8]);
            self.bits += n_bits;
            return;
        }
        for i in 0..n_bits {
            let bit = bytes[(start + i) / 8] & (0x80 >> ((start + i) % 8)) != 0;
            self.push_bit(bit);
        }
    }
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
    // The evaluator keeps its environment ([`Env`]) in Rust locals, so its terms are **not**
    // in the root set a delivery-driven collection can publish: a reply delivered to this
    // process while the evaluation waits would collect (and move) the heap under a term an
    // earlier reply produced. The whole driver therefore runs with the process marked
    // relocation-exempt, so such a delivery is refused and routed to the heap-fragment
    // fallback instead of relocating anything (see
    // [`RuntimeContext::with_relocation_exempt`]). The evaluator's own receive pump
    // (`Runtime::pump_scheduler`) runs **inside** this scope, which is exactly when a reply to
    // this very pid can arrive.
    ctx.with_relocation_exempt(|ctx| {
        let tree = parse::parse(expr)?;
        let mut evaluator = Evaluator { ctx };
        let mut env = Env::new();
        let outcome = evaluator.expr(&tree, &mut env);
        finish(outcome, evaluator.ctx)
    })
}

/// Converts the evaluator's control flow into a runtime [`Result`].
///
/// A [`Flow::Fatal`] is returned unchanged. A [`Flow::Raise`] that came from a BIF
/// (`origin`) is returned unchanged too; one the evaluator produced itself is
/// re-built as a [`BeamError::Raise`] from its live reason term.
///
/// # The structured reason
///
/// The evaluator's own raises (`{case_clause, Subject}`, `{badmatch, Value}`,
/// `{bad_generator, X}`, …) are **terms**, but [`ExitReason`](beam_common::ExitReason)
/// holds no term: [`beam_bifs::exit_reason_from_term`] maps a non-atom to
/// [`ExitReason::Structured`](beam_common::ExitReason::Structured). The term is therefore
/// attached to the raise's opaque
/// [`live_reason`](beam_common::Raise::live_reason) slot — the same convention
/// `beam_bifs` uses — so a **consumer** of the raise renders the real reason
/// (`error {case_clause,1}`) instead of the `<structured reason>` placeholder. The
/// consumers are the one recovery point [`crate::error::live_reason_term`] and everything
/// built on it: `cli.rs`'s `reported_reason` (the boot and `run`-file routes),
/// [`crate::runner`](crate::runner)'s `exit_reason` for a process that dies of the raise,
/// and this module's own [`Evaluator::flow_from_error`] when the raise re-enters the
/// evaluator. An action arm that prints a raise with `BeamError`'s `Display` instead of
/// going through that recovery point cannot show the term — the slot is what makes the
/// term *available*, not the CLI's rendering.
///
/// An **atom** reason gets no slot: the atom is the authoritative reason, so a term
/// attached next to one must never shadow it ([`crate::error::live_reason_term`]
/// refuses such a raise anyway, and the `unsupported`/`halt` classification reads the
/// atom). This is the **only** terminal conversion of an evaluator `Flow::Raise`, so
/// the one arm covers every raise site in this module it is called from.
fn finish(result: EResult<Eterm>, ctx: &RuntimeContext<'_>) -> Result<Eterm> {
    match result {
        Ok(value) => Ok(value),
        Err(Flow::Fatal(err)) => Err(err),
        Err(Flow::Raise {
            class,
            reason,
            origin,
            // The cooked trace (a `try … catch C:R:S` / `catch` binding) is rendered
            // where it is caught, not here; an *uncaught* raise reaches the CLI as the
            // raise's own `origin` error, exactly as before.
            head: _,
        }) => match origin {
            Some(err) => Err(err),
            None => {
                let exit_reason = beam_bifs::exit_reason_from_term(
                    reason,
                    ctx.runtime.code_server.atoms(),
                );
                let raise = Raise::new(class, exit_reason);
                // `ExitReason::as_atom` is the gate (not `BeamError::as_atom`): it answers
                // `None` for exactly the `Structured` reason whose term would otherwise be
                // lost, and `Some(..)` for every atom reason — which must keep behaving
                // exactly as it does today.
                let raise = if raise.reason.as_atom().is_some() {
                    raise
                } else {
                    raise.with_live_reason(LiveReason::new(reason))
                };
                Err(BeamError::Raise(raise))
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
            Expr::Int(value) => self.integer(*value),
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
    ///
    /// Evaluation order matches the oracle (`erl_eval`): the elements are evaluated
    /// **left-to-right**, then the optional tail, and only then is the cons chain built
    /// (from the already-evaluated values, right-to-left) — so observable side effects
    /// (an `io:format/2` inside an element) fire in source order and the tail's fire
    /// last. Building the chain from values keeps the list's *value* unchanged.
    fn list(&mut self, elements: &[Expr], tail: Option<&Expr>, env: &mut Env) -> EResult<Eterm> {
        let values = self.terms(elements, env)?;
        let mut acc = match tail {
            Some(tail) => self.expr(tail, env)?,
            None => NIL,
        };
        for value in values.into_iter().rev() {
            acc = heap!(terms::make_cons(self.ctx, value, acc));
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
        // Use the general, **canonical-order** map builder (not the insertion-order
        // flat-map writer), so a map literal's observable key order matches the oracle
        // (`maps:keys(#{3=>a,1=>b,2=>c})` → `[1,2,3]`) and a >32-key literal lifts to a
        // hashmap instead of hitting the flat-map cap. The atoms handle is an owned
        // `Rc` clone, so it does not conflict with the mutable `self.ctx` borrow.
        let atoms = self.ctx.runtime.code_server.atoms_handle();
        Ok(heap!(terms::make_map(self.ctx, &pairs, &atoms)))
    }

    /// `<<Seg, …>>`.
    fn binary(&mut self, segments: &[Seg], env: &mut Env) -> EResult<Eterm> {
        let mut buf = BitBuf::default();
        for segment in segments {
            self.append_segment(segment, env, &mut buf)?;
        }
        self.materialize_bits(&buf)
    }

    /// Materializes accumulated bits: a byte-aligned result is a **binary**, a
    /// non-byte-aligned one a **bitstring**.
    ///
    /// A small byte-aligned binary goes on the engine arena
    /// ([`terms::make_heap_binary`], which is limited to `ERL_ONHEAP_BINARY_LIMIT` — 64 —
    /// bytes); anything else — a non-byte-aligned bitstring, or a payload past that limit
    /// (e.g. `<<0:2048>>`) — is written by [`beam_codec::term::make_bitstring`] on the
    /// runtime's long-lived [`eval_heap`](crate::context::RuntimeContext::eval_heap),
    /// which has no size limit. Both are pointer-stable, so the term stays valid for the
    /// runtime's lifetime.
    fn materialize_bits(&mut self, buf: &BitBuf) -> EResult<Eterm> {
        if buf.bits % 8 == 0 && buf.bytes.len() <= 64 {
            Ok(heap!(terms::make_heap_binary(self.ctx, &buf.bytes)))
        } else {
            let heap = self.ctx.eval_heap();
            Ok(heap!(beam_codec::term::make_bitstring(
                heap, &buf.bytes, buf.bits
            )))
        }
    }

    /// Appends one binary segment's bits to `out`.
    fn append_segment(&mut self, segment: &Seg, env: &mut Env, out: &mut BitBuf) -> EResult<()> {
        let value = self.expr(&segment.value, env)?;
        match segment.kind {
            Some(SegKind::Utf8) => {
                // A utf-type segment with an explicit width is a *syntax* error on the
                // oracle, so it is a typed `badarg` here, never a fabricated encoding.
                if bits::utf_segment_has_width(segment) {
                    return Err(self.raise(ExceptionClass::Error, "badarg"));
                }
                return self.push_utf8(value, out);
            }
            Some(SegKind::Binary)
            | Some(SegKind::Bytes)
            | Some(SegKind::Bitstring)
            | Some(SegKind::Bits) => {
                return self.push_bitstring_segment(segment, value, env, out)
            }
            Some(SegKind::Float) => {
                // A float segment is an IEEE 754 field whose size defaults to 64 bits and is
                // one of `:16`/`:32`/`:64` (binary16/32/64); any other resolved width is the
                // oracle's `badarg` (`<<1.5:8/float>>`, `<<1.5:24/float>>`). The value may be
                // a float **or** an integer (small or bignum: `<<(1 bsl 64):64/float>>` is
                // `<<67,240,0,0,0,0,0,0>>`), and a non-number is `badarg`. `signed` is
                // meaningless here and ignored; `little`/`native` reverse the field's bytes.
                let bits = match self.segment_size_bits(segment, env, 1)? {
                    Some(bits) => bits,
                    None => 64,
                };
                let number = match terms::float_value(value) {
                    Ok(number) => number,
                    Err(_) => {
                        let (negative, limbs) = self.integer_magnitude(value)?;
                        bits::integer_limbs_to_f64(negative, &limbs)
                    }
                };
                let Some(bytes) =
                    bits::float_bytes(number, bits, integer_segment_is_little(segment))
                else {
                    return Err(self.raise(ExceptionClass::Error, "badarg"));
                };
                for byte in bytes {
                    out.push_byte(byte);
                }
                return Ok(());
            }
            Some(SegKind::Utf16) | Some(SegKind::Utf32) => {
                // A utf-type segment with an explicit width is a *syntax* error on the
                // oracle, so it is a typed `badarg` here, never a fabricated encoding.
                if bits::utf_segment_has_width(segment) {
                    return Err(self.raise(ExceptionClass::Error, "badarg"));
                }
                // A code point is a small integer; the encoders reject (with `None` ⇒
                // `badarg`) exactly what the oracle rejects: a non-integer, a lone surrogate
                // or anything above `16#10FFFF`.
                let (utf16, little) = (
                    segment.kind == Some(SegKind::Utf16),
                    integer_segment_is_little(segment),
                );
                let code = if is_small(value) {
                    Some(signed_value(value))
                } else {
                    None
                };
                let bytes = match code {
                    Some(code) if utf16 => bits::utf16_bytes(code, little),
                    Some(code) => bits::utf32_bytes(code, little),
                    None => None,
                };
                let Some(bytes) = bytes else {
                    return Err(self.raise(ExceptionClass::Error, "badarg"));
                };
                for byte in bytes {
                    out.push_byte(byte);
                }
                return Ok(());
            }
            Some(SegKind::Integer) | None => {}
        }
        // A string segment: a list of byte-sized integers with no size/type.
        if segment.size.is_none() && segment.kind.is_none() {
            if let Ok(elements) = terms::list_elements(value) {
                for element in elements {
                    out.push_byte(self.byte_of(element)?);
                }
                return Ok(());
            }
        }
        self.push_integer_segment(segment, value, env, out)
    }

    /// A `binary`/`bytes`/`bitstring`/`bits` segment: appends (part of) a bitstring value.
    ///
    /// `binary`/`bytes` require a byte-aligned value and default their unit to a byte;
    /// `bitstring`/`bits` accept a bitstring of any length and default their unit to a
    /// bit. An explicit `:Size` (times its `unit:`) is a bit count.
    fn push_bitstring_segment(
        &mut self,
        segment: &Seg,
        value: Eterm,
        env: &mut Env,
        out: &mut BitBuf,
    ) -> EResult<()> {
        let Ok((bytes, offset_bits, size_bits)) = beam_codec::term::bitstring_bytes(value) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        let in_bytes = matches!(segment.kind, Some(SegKind::Binary) | Some(SegKind::Bytes));
        if in_bytes && (offset_bits % 8 != 0 || size_bits % 8 != 0) {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        // The default unit is a byte for `binary`/`bytes`, a bit otherwise; an explicit
        // `unit:` replaces it, so the resolved size is always a bit count.
        let wanted = match self.segment_size_bits(segment, env, if in_bytes { 8 } else { 1 })? {
            Some(bits) => bits,
            None => size_bits,
        };
        if wanted > size_bits {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        out.push_bit_slice(bytes, offset_bits, wanted);
        Ok(())
    }

    /// Evaluates a bitstring-valued expression and appends **all** of its bits to `out`.
    ///
    /// This is the binary comprehension's template operation: `erl_eval` appends the
    /// template's value as a bitstring once per iteration
    /// (`eval_bc1/7`: `<<Acc/bitstring, V/bitstring>>`). A value that is not a bitstring
    /// is `badarg`.
    fn append_bitstring(&mut self, expr: &Expr, env: &mut Env, out: &mut BitBuf) -> EResult<()> {
        let value = self.expr(expr, env)?;
        let Ok((bytes, offset_bits, size_bits)) = beam_codec::term::bitstring_bytes(value) else {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        };
        out.push_bit_slice(bytes, offset_bits, size_bits);
        Ok(())
    }
    /// The explicit `:Size` of a segment as a **bit** count, or `None` when it has none.
    ///
    /// `default_unit` is the specifier's default unit in bits (1 for integer/`bits`,
    /// 8 for `binary`/`bytes`); an explicit `unit:N` specifier overrides it.
    fn segment_size_bits(
        &mut self,
        segment: &Seg,
        env: &mut Env,
        default_unit: u32,
    ) -> EResult<Option<usize>> {
        let Some(size) = &segment.size else {
            return Ok(None);
        };
        let size = self.expr(size, env)?;
        if !is_small(size) {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        let size = signed_value(size);
        if size < 0 {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        let unit = u128::from(segment.unit.unwrap_or(default_unit));
        let bits = (size as u128) * unit;
        usize::try_from(bits)
            .map(Some)
            .map_err(|_| Flow::Fatal(error::unsupported("segment size overflow")))
    }

    /// A numeric segment: `Value[:Size][/integer-…]`.
    ///
    /// An integer of **any** width is accepted — a small int or an exact bignum — so
    /// `<<0:2048>>` is 256 zero bytes and `<<(1 bsl 70):8>>` is `<<0>>`. The value is
    /// truncated to the segment's bit width exactly as the C's `erts_new_binary`/
    /// `bs_put_integer` do: two's complement, low `bits` first (`<<-1:2048>>` is 2048
    /// one-bits, `<<300:8>>` is `<<44>>`).
    fn push_integer_segment(
        &mut self,
        segment: &Seg,
        value: Eterm,
        env: &mut Env,
        out: &mut BitBuf,
    ) -> EResult<()> {
        let (negative, magnitude) = self.integer_magnitude(value)?;
        let bits = match self.segment_size_bits(segment, env, 1)? {
            Some(bits) => bits,
            None => 8,
        };
        // A zero-width segment contributes nothing (`<<1:0>>` is `<<>>`).
        if bits == 0 {
            return Ok(());
        }
        out.push_integer_bits(
            &magnitude,
            negative,
            bits,
            integer_segment_is_little(segment),
        );
        Ok(())
    }

    /// Encodes `value` as UTF-8 and appends the bytes.
    fn push_utf8(&mut self, value: Eterm, out: &mut BitBuf) -> EResult<()> {
        if !is_small(value) {
            return Err(self.raise(ExceptionClass::Error, "badarg"));
        }
        let code = u32::try_from(signed_value(value))
            .map_err(|_| self.raise(ExceptionClass::Error, "badarg"))?;
        let character = char::from_u32(code)
            .ok_or_else(|| self.raise(ExceptionClass::Error, "badarg"))?;
        let mut buffer = [0u8; 4];
        for byte in character.encode_utf8(&mut buffer).as_bytes() {
            out.push_byte(*byte);
        }
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
            UnOp::Bnot => self.call("erlang", "bnot", &[value]),
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
                // The six bit operators are `erlang` BIFs (`beam_bifs/src/arith.rs`):
                // dispatching to the registry gives their exact arbitrary-precision
                // semantics — a bignum operand, or a shift that overflows a small
                // integer, is handled there (`system_limit`/`badarith` come from the
                // registry, never re-raised here).
                let left = self.expr(lhs, env)?;
                let right = self.expr(rhs, env)?;
                let name = match op {
                    BinOp::Band => "band",
                    BinOp::Bor => "bor",
                    BinOp::Bxor => "bxor",
                    BinOp::Bsl => "bsl",
                    _ => "bsr",
                };
                self.call("erlang", name, &[left, right])
            }
            BinOp::Send => {
                // `Target ! Message`: evaluate the target, then the message, and deliver
                // through the host's `send_message` seam (which resolves the pid and copies
                // the message into the recipient's mailbox). The value of `!` is the message,
                // exactly as the oracle's is.
                let target = self.expr(lhs, env)?;
                let message = self.expr(rhs, env)?;
                match self.ctx.send_message(target, message) {
                    Ok(()) => Ok(message),
                    Err(err) => Err(self.flow_from_error(err)),
                }
            }
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
            head: None,
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
        // here (the semantics are exactly `erlang:error/1,2`, `exit/1` and
        // `throw/1`).
        //
        // `erlang:raise/3` is deliberately **not** served here: it must validate its
        // class *and* its stacktrace argument and forward the decoded trace, which is
        // exactly what the registered BIF does — so it falls through to
        // [`Self::call`] → `RuntimeContext::call_bif` → `beam_bifs::convert::raise_3`
        // (the oracle-pinned implementation) and reaches `Flow::Raise` through
        // [`Self::flow_from_error`], carrying both the live reason term and the
        // decoded stacktrace.
        if module == "erlang" {
            match (function, arity) {
                ("error", 1) | ("error", 2) => {
                    return Err(Flow::Raise {
                        class: ExceptionClass::Error,
                        reason: args[0],
                        origin: None,
                        head: None,
                    })
                }
                ("exit", 1) => {
                    return Err(Flow::Raise {
                        class: ExceptionClass::Exit,
                        reason: args[0],
                        origin: None,
                        head: None,
                    })
                }
                ("throw", 1) => {
                    return Err(Flow::Raise {
                        class: ExceptionClass::Throw,
                        reason: args[0],
                        origin: None,
                        head: None,
                    })
                }
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
                        head: None,
                    });
                }
                ("apply", 2) => return self.apply_2(&args),
                ("apply", 3) => return self.apply_3(&args),
                _ => {}
            }
        }
        self.call(module, function, &args)
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
    ///
    /// An **external** fun (`fun m:f/A`) is neither: it carries no `uniq`/`index`
    /// descriptor — its whole identity is `module/function/arity` — so it must skip the
    /// closure lookup (`terms::fun_uniq` rejects it with `BadArg`) and go straight to the
    /// host's ordinary dispatch, which is what
    /// [`RuntimeContext::apply_fun`](crate::context::RuntimeContext) does for it.
    fn apply_value(&mut self, function: Eterm, args: &[Eterm]) -> EResult<Eterm> {
        if terms::is_fun(function)
            && !matches!(
                heap!(beam_codec::term::fun_kind_term(function)),
                beam_codec::FunKind::External
            )
        {
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
            Err(err) => {
                let flow = self.flow_from_error(err);
                // An external `fun m:f/A` whose target does not exist raises `undef`
                // naming that callee; a local/imported fun exposes no module through
                // the term (its descriptor carries none), so nothing is synthesized
                // there rather than a guess.
                let flow = match (
                    beam_codec::term::fun_module(function),
                    beam_codec::term::fun_function(function),
                ) {
                    (Ok(module), Ok(name)) => {
                        self.with_callee_head_item(flow, make_atom(module), make_atom(name), args)
                    }
                    _ => flow,
                };
                Err(flow)
            }
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
            head: None,
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
                                head: None,
                            })
                        }
                    }
                }
            }
            Err(Flow::Fatal(err)) => Err(Flow::Fatal(err)),
            Err(Flow::Raise {
                class,
                reason,
                origin,
                head,
            }) => self.catch_clauses(&try_block.catches, class, reason, origin, head, env),
        };
        if !try_block.after.is_empty() {
            let mut after_env = env.clone();
            self.exprs(&try_block.after, &mut after_env)?;
        }
        outcome
    }

    /// Matches an exception against a `try`'s catch clauses.
    ///
    /// The 3rd (stacktrace) pattern of a clause is matched against the **cooked** trace,
    /// exactly as the oracle's `try … catch C:R:S -> … end` binds it: the raise's
    /// synthesized callee head item (an `undef`) followed by the supplied
    /// `erlang:raise/3` frames, rendered `[{M,F,A,Location}]` (see
    /// [`Evaluator::cook_supplied_trace`]). The trace is computed **lazily**, at most
    /// once, and only when some clause actually carries a stacktrace pattern, so a
    /// `catch C:R -> …` clause pays nothing.
    fn catch_clauses(
        &mut self,
        clauses: &[CatchClause],
        class: ExceptionClass,
        reason: Eterm,
        origin: Option<BeamError>,
        head: Option<Eterm>,
        env: &Env,
    ) -> EResult<Eterm> {
        let class_term = self.atom(class.as_str());
        let mut cooked_trace: Option<Eterm> = None;
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
                    if cooked_trace.is_none() {
                        cooked_trace = Some(self.cook_supplied_trace(&origin, head)?);
                    }
                    let trace = cooked_trace.expect("the cooked trace was just computed");
                    matched = self.match_pattern(pattern, trace, &mut clause_env)?;
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
        // No clause matched: re-raise the **same** exception, `origin` and the
        // synthesized head item included, so an outer `catch`/`try` still sees the
        // supplied stacktrace.
        Err(Flow::Raise {
            class,
            reason,
            origin,
            head,
        })
    }

    /// `receive … end`.
    ///
    /// The `-eval` evaluator scans the running process's **real mailbox** (the same one the
    /// engine's compiled `receive` scans): the first message a clause's pattern and guards
    /// match is removed and its clause body evaluated. A message that matches nothing stays
    /// in the mailbox (that is the BEAM's selective receive).
    ///
    /// Because `-eval` runs *outside* a scheduler slice, a message that has not arrived yet
    /// has a meaning here too: the scheduler is **pumped** (bounded — see
    /// [`RECEIVE_PUMP_SLICES`]) so a process `erlang:spawn/1` started runs and can deliver.
    /// That is what makes `spawn(fun() -> Self ! hello end), receive M -> M after 1000 -> …`
    /// work end to end.
    ///
    /// * **with `after T`** — the mailbox is scanned, then the scheduler pumped, then the
    ///   remaining time is spent on the two *other* message sources: the port poller (the
    ///   shape `open_port({spawn, …}, …)` output arrives through) and the runtime's own
    ///   **timer heap** ([`Runtime::wait_for_timer`], the shape `erlang:send_after/3` and
    ///   `timer:apply_after/4` messages arrive through). The `after` body runs once the
    ///   deadline passes with no match. The whole wait is bounded by that one deadline — a
    ///   window with no pending deadline is slept out, so `receive _ -> ok after 300 -> ok
    ///   end` takes the oracle's 300 ms rather than returning at once — and the loop is
    ///   driven by real deadlines only, so the case with nothing to run, no port output and
    ///   no timer cannot busy-spin.    /// * **without `after`** — a blocking receive. It pumps the scheduler while anything is
    ///   runnable and then reports the typed capability gap `receive: no matching message
    ///   arrived` rather than hanging (there is no host timer to block on in Milestone 1).
    fn receive(&mut self, receive: &ast::Receive, env: &mut Env) -> EResult<Eterm> {
        match &receive.after {
            Some((timeout, body)) => {
                let mut timeout_env = env.clone();
                let value = self.expr(timeout, &mut timeout_env)?;
                let millis = self.timeout_millis(value)?;
                let deadline = Instant::now() + Duration::from_millis(millis);
                loop {
                    if let Some((index, bound)) = self.try_mailbox_receive(&receive.clauses, env)? {
                        let mut clause_env = bound;
                        return self.exprs(&receive.clauses[index].body, &mut clause_env);
                    }
                    // A runnable process (a `spawn`ed child, say) may deliver the message we
                    // are waiting for: pump it and rescan. `0` slices means nothing was
                    // runnable, so the mailbox cannot have changed.
                    if self.ctx.may_pump_scheduler() {
                        let ran = heap!(self.ctx.runtime.pump_scheduler(RECEIVE_PUMP_SLICES));
                        if ran > 0 {
                            continue;
                        }
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    // The port poller owns the *other* message source (see the module docs of
                    // `crate::context`): its messages are produced, matched and discarded, not
                    // enqueued, so a non-matching one is not left behind. With a live port it
                    // waits out the remaining time; with none it returns at once.
                    let messages = match self.ctx.poll_port_messages(remaining) {
                        Ok(messages) => messages,
                        Err(err) => return Err(Flow::Fatal(err)),
                    };
                    for message in messages {
                        if let Some(result) = self.match_clauses(&receive.clauses, message, env)? {
                            return Ok(result);
                        }
                    }
                    // The **third** source is the runtime's own timer heap, and only a
                    // scheduler pass fires it: `timer:apply_after/4` (a `timer_server` arming
                    // `erlang:start_timer/3`) and `erlang:send_after/3` deliver through it.
                    // Waiting for the earliest pending deadline — or, with none, for the rest
                    // of this window — and then pumping is what makes
                    // `timer:apply_after(300, erlang, send, [Self, done]), receive M -> M
                    // after 1500 -> timeout end` answer `done` here exactly as the oracle
                    // does, and what makes a bare `receive _ -> ok after 300 -> ok end` take
                    // its 300 ms instead of returning immediately. A context that must not
                    // pump (the evaluator running *inside* a scheduler slice) keeps the old
                    // immediate `after`: it may not re-enter the scheduler.
                    if !self.ctx.may_pump_scheduler() {
                        break;
                    }
                    if !heap!(self.ctx.runtime.wait_for_timer(deadline, RECEIVE_PUMP_SLICES)) {
                        // Nothing was pending inside the window and nothing became due: the
                        // window itself has elapsed, so run the `after` body.
                        break;
                    }
                }                let mut body_env = env.clone();
                self.exprs(body, &mut body_env)
            }
            None => loop {
                if let Some((index, bound)) = self.try_mailbox_receive(&receive.clauses, env)? {
                    let mut clause_env = bound;
                    return self.exprs(&receive.clauses[index].body, &mut clause_env);
                }
                // A bare receive inside a spawned process cannot pump the scheduler it is
                // itself running under (that would run a second process from within a slice),
                // and there is no host timer to block on — a typed gap, never a hang.
                if !self.ctx.may_pump_scheduler() {
                    return Err(Flow::Fatal(error::unsupported(
                        "receive (no after clause) inside a spawned process",
                    )));
                }
                let ran = heap!(self.ctx.runtime.pump_scheduler(RECEIVE_PUMP_SLICES));
                if ran == 0 {
                    return Err(Flow::Fatal(error::unsupported(
                        "receive: no matching message arrived",
                    )));
                }
            },
        }
    }

    /// The first clause of `clauses` that matches `value` (pattern, then guards), with the
    /// environment it bound and the clause's index.
    ///
    /// The clause **body is not evaluated** — the caller decides when to run it (a `receive`
    /// removes the message first, the BEAM's `remove_message` ordering).
    fn select_clause(
        &mut self,
        clauses: &[Clause],
        value: Eterm,
        env: &Env,
    ) -> EResult<Option<(usize, Env)>> {
        for (index, clause) in clauses.iter().enumerate() {
            let mut clause_env = env.clone();
            if let Some(pattern) = &clause.pattern {
                if !self.match_pattern(pattern, value, &mut clause_env)? {
                    continue;
                }
            }
            if !self.guards_ok(&clause.guards, &mut clause_env)? {
                continue;
            }
            return Ok(Some((index, clause_env)));
        }
        Ok(None)
    }

    /// Scans the running process's mailbox for the first message a clause matches.
    ///
    /// On a match the message is **removed from the mailbox** (before the body runs, the
    /// BEAM's `remove_message` ordering) and `(clause_index, bound_env)` is returned.
    /// `Ok(None)` means no message in the mailbox matches — every message stays queued.
    fn try_mailbox_receive(
        &mut self,
        clauses: &[Clause],
        env: &Env,
    ) -> EResult<Option<(usize, Env)>> {
        let messages = match self.ctx.mailbox_messages() {
            Ok(messages) => messages,
            Err(err) => return Err(self.flow_from_error(err)),
        };
        for (message_index, message) in messages.iter().enumerate() {
            if let Some((clause_index, bound)) = self.select_clause(clauses, *message, env)? {
                if let Err(err) = self.ctx.remove_mailbox_message(message_index) {
                    return Err(self.flow_from_error(err));
                }
                return Ok(Some((clause_index, bound)));
            }
        }
        Ok(None)
    }
    /// A `receive … after T` timeout as a millisecond count.
    ///
    /// A negative value, a float or any other term is `error:badarg` (the oracle's own
    /// reason for those). A value that is an integer but too wide to be a *small* integer (a
    /// bignum) is also `badarg` — the evaluator has no safe bignum reader here and a
    /// multi-quintillion-millisecond timeout is not representable anyway. The atom
    /// `infinity` would block forever, which `-eval` (no mailbox) cannot do, so it is a
    /// capability gap, not an error.
    fn timeout_millis(&mut self, value: Eterm) -> EResult<u64> {
        if is_small(value) {
            let millis = signed_value(value);
            if millis < 0 {
                return Err(self.raise(ExceptionClass::Error, "badarg"));
            }
            return Ok(millis as u64);
        }
        if self.atom_name(value).as_deref() == Some("infinity") {
            return Err(Flow::Fatal(error::unsupported(
                "receive ... after infinity (no mailbox in -eval)",
            )));
        }
        Err(self.raise(ExceptionClass::Error, "badarg"))
    }

    /// `catch Expr`.
    ///
    /// Folds **by class**, the literal `erl_eval` rule (`lib/stdlib/src/erl_eval.erl`):
    /// a `throw` yields the reason, an `exit` yields `{'EXIT', Reason}` (never a trace,
    /// even if one was supplied), and an `error` yields `{'EXIT', {Reason, Trace}}`
    /// **always** — a 2-tuple whose trace is the raise's synthesized callee head item
    /// (an `undef`) followed by its supplied `erlang:raise/3` frames (or `[]`).
    fn catch_expr(&mut self, inner: &Expr, env: &mut Env) -> EResult<Eterm> {
        match self.expr(inner, env) {
            Ok(value) => Ok(value),
            Err(Flow::Fatal(err)) => Err(Flow::Fatal(err)),
            Err(Flow::Raise {
                class,
                reason,
                origin,
                head,
            }) => match class {
                ExceptionClass::Throw => Ok(reason),
                ExceptionClass::Error => {
                    let trace = self.cook_supplied_trace(&origin, head)?;
                    let tag = self.atom("EXIT");
                    let reason = heap!(terms::make_tuple(self.ctx, &[reason, trace]));
                    Ok(heap!(terms::make_tuple(self.ctx, &[tag, reason])))
                }
                _ => {
                    let tag = self.atom("EXIT");
                    Ok(heap!(terms::make_tuple(self.ctx, &[tag, reason])))
                }
            },
        }
    }

    /// Cooks a raise's trace into the oracle's `[{M,F,A,Location}]` list.
    ///
    /// Two sources, innermost first:
    ///
    /// * the raise's **synthesized callee head item** (`head`) — the one item the
    ///   tree-walker can honestly contribute, built at the failing call site of an
    ///   `undef` (see [`Evaluator::with_callee_head_item`]); and
    /// * the frames an `erlang:raise/3` **supplied** (`raise.stacktrace`), rendered by
    ///   the engine's public
    ///   [`encode_supplied_stacktrace`](beam_engine::encode_supplied_stacktrace) exactly
    ///   as the oracle renders it (innermost first).
    ///
    /// A raise with neither — `erlang:error/1`, an arithmetic error — cooks to [`NIL`],
    /// the empty trace. The walker renders **no** `erl_eval`/`init` frame of its own:
    /// there is no `erl_eval` port here, and a fabricated frame would be a wrong term.
    ///
    /// The `module` argument is an **atom source only** — resolved by lookup, nothing is
    /// interned — so it carries a **clone** of the runtime's shared atom table (built
    /// after the raise's frame names were already interned by the `-eval` expression), and
    /// a name the table cannot resolve surfaces as the typed `error::unsupported(...)`,
    /// never a substituted atom.
    fn cook_supplied_trace(
        &mut self,
        origin: &Option<BeamError>,
        head: Option<Eterm>,
    ) -> EResult<Eterm> {
        let mut trace = match origin {
            Some(BeamError::Raise(raise)) => {
                let module = beam_loader::Module {
                    atom_table: self.ctx.runtime.code_server.atoms().clone(),
                    ..beam_loader::Module::default()
                };
                let ctx: &mut dyn ExecutionContext = &mut *self.ctx;
                heap!(beam_engine::encode_supplied_stacktrace(
                    &module,
                    ctx,
                    &raise.stacktrace
                ))
            }
            _ => NIL,
        };
        if let Some(item) = head {
            trace = heap!(terms::make_cons(self.ctx, item, trace));
        }
        Ok(trace)
    }

    /// `fun Name/Arity` / `fun Mod:Name/Arity` — a local `fun f/A` is `erlang:f/A`.
    ///
    /// The result is the real [`FunKind::External`](beam_codec::FunKind::External) term
    /// (the `EXPORT_EXT` encoding), **not** one of this evaluator's closures: `fun m:f/A`
    /// captures no environment and its whole identity is `module/function/arity`, so the
    /// term *is* the fun — `term_to_binary/1` encodes it as `EXPORT_EXT` and
    /// [`RuntimeContext::apply_fun`](crate::context::RuntimeContext) applies it through the
    /// ordinary dynamic call, exactly as the oracle does.
    fn fun_ref(&mut self, module: Option<&str>, name: &str, arity: u32) -> EResult<Eterm> {
        let module = self.atom(module.unwrap_or("erlang"));
        let function = self.atom(name);
        let heap = self.ctx.eval_heap();
        Ok(heap!(beam_codec::term::make_fun_external(
            heap,
            arity as usize,
            atom_val(module),
            atom_val(function),
        )))
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
            CompTemplate::Binary(_) => CompOut::Bits(BitBuf::default()),
        };
        self.comp_collect(&template, quals, 0, env, &mut out)?;
        match out {
            CompOut::List(elements) => Ok(heap!(terms::make_list(self.ctx, &elements))),
            CompOut::Map(pairs) => {
                // The canonical-order builder (see the `#{…}` literal): the map
                // comprehension's key order must match the oracle's, not insertion order.
                let atoms = self.ctx.runtime.code_server.atoms_handle();
                Ok(heap!(terms::make_map(self.ctx, &pairs, &atoms)))
            }
            CompOut::Bits(buf) => self.materialize_bits(&buf),
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
                    // A binary comprehension's template is the `<<…>>` *expression* that
                    // follows the opening bracket (`erl_parse.yrl`:
                    // `binary_comprehension -> '<<' expr_max '||' …`), and `erl_eval`
                    // appends *its value* as a bitstring once per iteration
                    // (`eval_bc1/7`: `<<Acc/bitstring, V/bitstring>>`). Our parser records
                    // that expression as the comprehension's single segment, so evaluate
                    // the segment's value and append it as a bitstring.
                    if let Some(segment) = segments.first() {
                        if let CompOut::Bits(buffer) = out {
                            self.append_bitstring(&segment.value, env, buffer)?;
                        }
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
            Qual::BinaryGenerator { pattern, source } => {
                let Expr::Binary(segments) = pattern else {
                    return Err(Flow::Fatal(error::message(
                        "a binary generator needs a <<…>> pattern",
                    )));
                };
                let source = self.expr(source, env)?;
                let Ok((bytes, offset_bits, size_bits)) = beam_codec::term::bitstring_bytes(source)
                else {
                    // The source is not a bitstring: the oracle raises
                    // `{bad_generator, X}` for the offending value, so the reason is a
                    // live term rather than a bare atom.
                    return Err(Flow::Raise {
                        class: ExceptionClass::Error,
                        reason: self.bad_generator(source)?,
                        origin: None,
                        head: None,
                    });
                };
                // The pattern may materialize terms (a `binary` sub-segment) on the heaps
                // the source lives on, so own the payload rather than borrowing it.
                let bytes = bytes.to_vec();
                let mut consumed = 0usize;
                while consumed < size_bits {
                    let mut branch = env.clone();
                    match self.match_binary_segments(
                        segments,
                        &bytes,
                        offset_bits + consumed,
                        size_bits - consumed,
                        &mut branch,
                    )? {
                        // A match must advance; a zero-width match would spin forever, so
                        // it terminates the generator (as a failing match does).
                        Some(step) if step > 0 => {
                            self.comp_collect(template, quals, index + 1, &mut branch, out)?;
                            consumed += step;
                        }
                        _ => break,
                    }
                }
                Ok(())
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
        // A non-iterable generator source raises `{bad_generator, X}` (the offending
        // value), matching the oracle for both the list and the binary generator forms.
        Err(Flow::Raise {
            class: ExceptionClass::Error,
            reason: self.bad_generator(value)?,
            origin: None,
            head: None,
        })
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

    /// Matches a `<<…>>` pattern against a bitstring value, consuming it whole.
    fn match_binary(&mut self, segments: &[Seg], value: Eterm, env: &mut Env) -> EResult<bool> {
        let Ok((bytes, offset_bits, size_bits)) = beam_codec::term::bitstring_bytes(value) else {
            return Ok(false);
        };
        // The pattern may build terms on the heaps the value lives on, so own the
        // payload rather than holding the borrow across those allocations.
        let bytes = bytes.to_vec();
        match self.match_binary_segments(segments, &bytes, offset_bits, size_bits, env)? {
            Some(consumed) => Ok(consumed == size_bits),
            None => Ok(false),
        }
    }

    /// Matches `<<Segments>>` against `available_bits` bits of `bytes` starting at bit
    /// `offset_bits`.
    ///
    /// Returns the number of bits the pattern consumed, or `None` when a segment does
    /// not match (including when not enough bits remain) — an Erlang pattern-match
    /// failure, never an exception. Every segment type is supported: an integer of any
    /// width, a `float` field of 16/32/64 bits, a `binary`/`bytes`/`bitstring`/`bits`
    /// slice, and `utf8`/`utf16`/`utf32` (an unsized `binary`/`bitstring` segment takes the
    /// remaining bits and must be last). A segment the oracle itself rejects — a `float`
    /// width other than 16/32/64, a malformed or truncated UTF sequence, a value it does not
    /// accept — is a match failure too ([`bits_decode`]), never a raise.
    fn match_binary_segments(
        &mut self,
        segments: &[Seg],
        bytes: &[u8],
        offset_bits: usize,
        available_bits: usize,
        env: &mut Env,
    ) -> EResult<Option<usize>> {
        let mut consumed = 0usize;
        for (index, segment) in segments.iter().enumerate() {
            let remaining = available_bits - consumed;
            match segment.kind {
                None | Some(SegKind::Integer) => {
                    let bits = match self.segment_size_bits(segment, env, 1)? {
                        Some(bits) => bits,
                        None => 8,
                    };
                    if bits > remaining {
                        return Ok(None);
                    }
                    let term = self.integer_segment_term(bytes, offset_bits + consumed, bits, segment)?;
                    if !self.match_pattern(&segment.value, term, env)? {
                        return Ok(None);
                    }
                    consumed += bits;
                }
                Some(SegKind::Float) => {
                    // An IEEE 754 field: `:16`/`:32`/`:64` (binary16/32/64) or, with no
                    // `:Size`, the default 64 bits. `signed` is meaningless for a float and
                    // ignored; `little`/`native` reverse the field's whole byte image.
                    let bits = match self.segment_size_bits(segment, env, 1)? {
                        Some(bits) => bits,
                        None => 64,
                    };
                    if bits > remaining {
                        return Ok(None);
                    }
                    // A width the oracle does not accept (`<<F:24/float>>`) fails the match
                    // (`erts_bs_get_float_2`), rather than raising.
                    let Some(value) = bits_decode::decode_float(
                        bytes,
                        offset_bits + consumed,
                        bits,
                        integer_segment_is_little(segment),
                    ) else {
                        return Ok(None);
                    };
                    let term = heap!(terms::make_float(self.ctx, value));
                    if !self.match_pattern(&segment.value, term, env)? {
                        return Ok(None);
                    }
                    consumed += bits;
                }
                Some(SegKind::Utf8) | Some(SegKind::Utf16) | Some(SegKind::Utf32) => {
                    // A utf-type pattern segment with an explicit `:Size` or `unit:` does not
                    // even parse on the oracle (`<<C:8/utf8>>` is a syntax error), so no
                    // differential case can exist for one: report the typed capability gap
                    // rather than fabricate a width.
                    if bits::utf_segment_has_width(segment) {
                        return Err(Flow::Fatal(error::unsupported(
                            "a utf binary pattern segment with an explicit size or unit",
                        )));
                    }
                    let little = integer_segment_is_little(segment);
                    let decoded = match segment.kind {
                        Some(SegKind::Utf8) => {
                            bits_decode::decode_utf8(bytes, offset_bits + consumed)
                        }
                        Some(SegKind::Utf16) => {
                            bits_decode::decode_utf16(bytes, offset_bits + consumed, little)
                        }
                        _ => bits_decode::decode_utf32(bytes, offset_bits + consumed, little),
                    };
                    // An overlong encoding, a surrogate, a code point past `16#10FFFF`, a
                    // stray continuation byte or a truncated sequence does not match.
                    let Some((code, bits)) = decoded else {
                        return Ok(None);
                    };
                    // The decoder may look past the value's own window (the payload's last
                    // byte can carry bits that are not part of it), so the sequence it read
                    // must still fit in the bits that remain.
                    if bits > remaining {
                        return Ok(None);
                    }
                    let term = self.small(i64::from(code))?;
                    if !self.match_pattern(&segment.value, term, env)? {
                        return Ok(None);
                    }
                    consumed += bits;
                }
                Some(SegKind::Binary) | Some(SegKind::Bytes) | Some(SegKind::Bitstring)
                | Some(SegKind::Bits) => {
                    let in_bytes =
                        matches!(segment.kind, Some(SegKind::Binary) | Some(SegKind::Bytes));
                    let bits = match self
                        .segment_size_bits(segment, env, if in_bytes { 8 } else { 1 })?
                    {
                        Some(bits) => bits,
                        None => {
                            if index + 1 != segments.len() {
                                return Err(Flow::Fatal(error::unsupported(
                                    "a non-final unsized binary pattern segment",
                                )));
                            }
                            remaining
                        }
                    };
                    if bits > remaining {
                        return Ok(None);
                    }
                    let sub = self.bit_slice_term(bytes, offset_bits + consumed, bits)?;
                    if !self.match_pattern(&segment.value, sub, env)? {
                        return Ok(None);
                    }
                    consumed += bits;
                }
            }
        }
        Ok(Some(consumed))
    }

    /// The integer term an integer pattern segment of `bits` bits denotes.
    ///
    /// The field is read in the segment's own endianness (`little`/`native` versus
    /// `big`, [`integer_segment_is_little`]) and is exact for **any** width — one that is
    /// neither a multiple of 8 nor of 64 included — so `<<A:65>>`, `<<A:72>>`,
    /// `<<A:120>>`, `<<A:2/unit:8>>` and a `signed` field wider than 64 bits all match
    /// exactly as the oracle does. A width of `0` is the integer `0`.
    ///
    /// `signed` reinterprets the field as a two's-complement integer of exactly `bits`
    /// bits (`erts_bs_get_integer_2`'s sign extension, applied over the segment's own
    /// width rather than over 64 bits): a set top bit makes the value `-(2^bits - raw)`.
    ///
    /// The value is materialised as a small int, a one-limb bignum or an exact multi-limb
    /// bignum ([`terms::make_integer`], else [`beam_codec::term::make_big`]) — **never** a
    /// `system_limit` raise and never a wrong term, because a `<<A:60>>`-shaped segment is
    /// a bignum in OTP too.
    fn integer_segment_term(
        &mut self,
        bytes: &[u8],
        start: usize,
        bits: usize,
        segment: &Seg,
    ) -> EResult<Eterm> {
        // A zero-width field (`<<A:0>>`) has no bits to read: it is the integer `0`.
        if bits == 0 {
            return Ok(heap!(terms::make_integer(self.ctx, 0)));
        }
        let little = integer_segment_is_little(segment);
        let signed = segment.signed == Some(true);
        // A field of at most one word — the overwhelmingly common case — is read straight
        // into a `u64`: `read_bits_be` is bit-exact at any width, and the byte-wise
        // `read_bits_le` at a whole number of bytes. Elsewhere — a width past a word, or a
        // little-endian width whose trailing *partial* byte holds the field's high bits —
        // [`bits_decode::read_integer_limbs`] supplies the magnitude limbs, and it is exact
        // for every width.
        let (magnitude, negative) = if bits <= 64 && (!little || bits % 8 == 0) {
            let raw = if little {
                read_bits_le(bytes, start, bits)
            } else {
                read_bits_be(bytes, start, bits)
            };
            if signed {
                let value = sign_extend(raw, bits);
                (vec![value.unsigned_abs()], value < 0)
            } else {
                // An unsigned 64-bit field with its top bit set is a one-limb bignum, which
                // the materialisation below builds from this very magnitude.
                (vec![raw], false)
            }
        } else {
            let mut limbs = bits_decode::read_integer_limbs(bytes, start, bits, little);
            // A `signed` segment whose top (bit `bits - 1`) bit is set denotes the negative
            // of its two's-complement magnitude: negate within the field's own width.
            let negative = signed && (limbs[(bits - 1) / 64] >> ((bits - 1) % 64)) & 1 == 1;
            if negative {
                for limb in &mut limbs {
                    *limb = !*limb;
                }
                if bits % 64 != 0 {
                    // The top limb is partial: drop the bits above the field before adding.
                    let mask = (1u64 << (bits % 64)) - 1;
                    if let Some(top) = limbs.last_mut() {
                        *top &= mask;
                    }
                }
                let mut carry = true;
                for limb in &mut limbs {
                    if !carry {
                        break;
                    }
                    let (sum, overflow) = limb.overflowing_add(1);
                    *limb = sum;
                    carry = overflow;
                }
            }
            (limbs, negative)
        };
        let digits = trim_digits(&magnitude);
        // A one-limb magnitude in `i64` range is exactly what `make_integer` materialises
        // (a small int, or the oracle's one-limb bignum past the 60-bit small range).
        if digits.len() == 1 {
            let magnitude = digits[0];
            let limit = if negative {
                1u64 << 63
            } else {
                i64::MAX as u64
            };
            if magnitude <= limit {
                let value = if negative {
                    (magnitude as i64).wrapping_neg()
                } else {
                    magnitude as i64
                };
                return Ok(heap!(terms::make_integer(self.ctx, value)));
            }
        }
        let heap = self.ctx.eval_heap();
        Ok(heap!(beam_codec::term::make_big(heap, negative, digits)))
    }

    /// The bitstring term for `bits` bits of `bytes` starting at `start`.
    fn bit_slice_term(&mut self, bytes: &[u8], start: usize, bits: usize) -> EResult<Eterm> {
        let mut buf = BitBuf::default();
        buf.push_bit_slice(bytes, start, bits);
        self.materialize_bits(&buf)
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
    ///
    /// A failure that is the bare `error:undef` — the host could not resolve the call
    /// target — additionally carries the synthesized callee head item (see
    /// [`Evaluator::with_callee_head_item`]), so a caught raise renders the oracle's
    /// innermost `{Module, Function, ArgList, []}` trace item.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> EResult<Eterm> {
        match self
            .ctx
            .call_bif(module, function, args.len() as u32, args)
        {
            Ok(value) => Ok(value),
            Err(err) => {
                let flow = self.flow_from_error(err);
                let flow = self.with_callee_head_str(flow, module, function, args);
                Err(flow)
            }
        }
    }

    /// [`Evaluator::with_callee_head_item`] for a call named by its `module`/`function`
    /// **strings** (interned here); the atoms are already interned for an `apply/3` whose
    /// module/function came from a term, so interning is a no-op lookup there.
    fn with_callee_head_str(
        &mut self,
        flow: Flow,
        module: &str,
        function: &str,
        args: &[Eterm],
    ) -> Flow {
        if !self.is_undef_raise(&flow) {
            return flow;
        }
        let module = self.atom(module);
        let function = self.atom(function);
        self.with_callee_head_item(flow, module, function, args)
    }

    /// Attaches the **synthesized callee head item** — `{Module, Function, ArgList, []}`
    /// — to a bare-`error:undef` raise, so a caught `try … catch C:R:S` (or a bare
    /// `catch`) renders the oracle's innermost trace item.
    ///
    /// The oracle's `undef` names its missing callee **only** through the trace: C's
    /// `build_stacktrace` puts the failing call's **real** argument list in the item's
    /// `A` slot (see `crates/beam_engine/src/families/exceptions/callee.rs`, whose
    /// compiled-code path synthesizes the same item). The tree-walker is the failing call
    /// site here, so it is the one place that knows the callee identity and its evaluated
    /// arguments.
    ///
    /// * `module`/`function` are **atom immediates** and `args` the **real** argument
    ///   list, so nothing is fabricated: no `erl_eval`/`init` frame is invented (there is
    ///   no `erl_eval` port), and `Location` is the empty list exactly as the oracle's is
    ///   for a callee that does not exist.
    /// * It applies only to the bare `undef` ([`Evaluator::is_undef_raise`]) and only when
    ///   no head item is present yet, so every other raise keeps today's trace and a
    ///   re-raised `undef` keeps the item it already carries.
    ///
    /// A failure to build the item (heap exhaustion) leaves the raise's class/reason
    /// untouched — today's empty trace — rather than masking the real exception with a
    /// `system_limit`.
    fn with_callee_head_item(
        &mut self,
        flow: Flow,
        module: Eterm,
        function: Eterm,
        args: &[Eterm],
    ) -> Flow {
        let Flow::Raise {
            class,
            reason,
            origin,
            head: None,
        } = flow
        else {
            return flow;
        };
        if !self.is_bare_undef_start(class, reason) {
            return Flow::Raise {
                class,
                reason,
                origin,
                head: None,
            };
        }
        match self.callee_head_item(module, function, args) {
            Ok(item) => Flow::Raise {
                class,
                reason,
                origin,
                head: Some(item),
            },
            Err(_) => Flow::Raise {
                class,
                reason,
                origin,
                head: None,
            },
        }
    }

    /// Whether `flow` is the bare `error:undef` raise a synthesized head item belongs to.
    fn is_undef_raise(&self, flow: &Flow) -> bool {
        matches!(
            flow,
            Flow::Raise { class, reason, head: None, .. }
                if self.is_bare_undef_start(*class, *reason)
        )
    }

    /// Whether a raise of `class` with `reason` is the bare `undef` — class `error` and
    /// the reason **atom** `undef` (never a structured reason, and never another class).
    fn is_bare_undef_start(&self, class: ExceptionClass, reason: Eterm) -> bool {
        class == ExceptionClass::Error && self.atom_name(reason).as_deref() == Some("undef")
    }

    /// The item `{Module, Function, ArgList, []}` for the callee `module:function` applied
    /// to `args` — one 4-element tuple plus one cons cell per argument.
    fn callee_head_item(
        &mut self,
        module: Eterm,
        function: Eterm,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let arg_list = terms::make_list(self.ctx, args)?;
        terms::make_tuple(self.ctx, &[module, function, arg_list, NIL])
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
    /// genuine `error`/`exit`/`throw` stays fully catchable.
    ///
    /// A **structured** reason — one `beam_bifs` carries as a live term
    /// (`{badmap, M}`, `{badkey, K}`, `{case_clause, V}`, …) in the raise's
    /// [`live_reason`](beam_common::Raise::live_reason) slot — is a genuine
    /// Erlang exception and is converted to a **catchable** [`Flow::Raise`]
    /// carrying that exact term. Its term was built on a pooled BIF heap that
    /// Milestone 1 never resets and never moves, so it stays valid for the
    /// runtime's lifetime; no copy is needed. Only an error that carries *no*
    /// term at all is [`Flow::Fatal`] rather than silently reshaped.
    fn flow_from_error(&mut self, err: BeamError) -> Flow {
        if error::halt_status(&err).is_some() {
            return Flow::Fatal(err);
        }
        if error::unsupported_reason(&err).is_some() {
            return Flow::Fatal(err);
        }
        // A structured reason is a real exception: recover its live term and raise it
        // catchably (the `beam_bifs` `raise.live_reason().downcast_ref::<Eterm>()`
        // convention). This runs before the atom-only mapping below, so a tag atom
        // (`badkey`/`badmap`) never shadows the term the producer built.
        if let BeamError::Raise(raise) = &err {
            let live_term = raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
                .copied();
            if let Some(reason) = live_term {
                let class = raise.class;
                return Flow::Raise {
                    class,
                    reason,
                    origin: Some(err),
                    head: None,
                };
            }
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
                    head: None,
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

    /// An integer literal term: a small int when it fits, an **exact bignum** otherwise.
    ///
    /// A value in `i64` range is materialised by [`terms::make_integer`] (a small int or a
    /// one-limb bignum — `9223372036854775807` is the oracle's `SMALL_BIG_EXT`); a value
    /// past `i64::MAX`/below `i64::MIN` (reachable only because the parser keeps an `i128`)
    /// needs at most two `ErtsDigit`s (`u128` magnitude). The literal is exact up to
    /// `i128`; anything wider was already rejected as a typed capability gap by the parser.
    fn integer(&mut self, value: i128) -> EResult<Eterm> {
        if let Ok(value) = i64::try_from(value) {
            return Ok(heap!(terms::make_integer(self.ctx, value)));
        }
        let magnitude = value.unsigned_abs();
        let digits = [
            (magnitude & u128::from(u64::MAX)) as u64,
            (magnitude >> 64) as u64,
        ];
        let heap = self.ctx.eval_heap();
        Ok(heap!(beam_codec::term::make_big(
            heap,
            value < 0,
            trim_digits(&digits)
        )))
    }

    /// The sign and magnitude limbs (least significant `ErtsDigit` first) of an integer
    /// term — a small int or an exact bignum; a non-integer is `error:badarg`.
    fn integer_magnitude(&mut self, value: Eterm) -> EResult<(bool, Vec<u64>)> {
        if is_small(value) {
            let value = signed_value(value);
            return Ok((value < 0, vec![value.unsigned_abs()]));
        }
        match beam_codec::term::big_parts(value) {
            Ok((negative, digits)) => Ok((negative, digits.to_vec())),
            Err(_) => Err(self.raise(ExceptionClass::Error, "badarg")),
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
            head: None,
        }
    }

    /// The `{bad_generator, X}` reason a comprehension generator raises for a source that
    /// cannot be iterated (`[Y || Y <- X]` / `<<… || <<…>> <= X>>` on a non-list /
    /// non-bitstring `X`) — the oracle's exact reason, with `X` the offending value.
    fn bad_generator(&mut self, value: Eterm) -> EResult<Eterm> {
        let tag = self.atom("bad_generator");
        Ok(heap!(terms::make_tuple(self.ctx, &[tag, value])))
    }

    /// `value` as a boolean, or `error:badarg`.
    fn boolean(&mut self, value: Eterm) -> EResult<bool> {
        match self.atom_name(value).as_deref() {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(self.raise(ExceptionClass::Error, "badarg")),
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

/// Whether an integer segment is stored least-significant byte first.
///
/// `little` and `native` both mean little-endian on the supported platforms (`native`
/// tracks the machine order, which is little-endian on x86-64/aarch64); a missing or
/// `big` specifier is big-endian.
fn integer_segment_is_little(segment: &Seg) -> bool {
    matches!(
        segment.endian,
        Some(ast::Endian::Little) | Some(ast::Endian::Native)
    )
}

/// Reads `n` bits (`n <= 64`) starting at bit `start` as a **big-endian** unsigned
/// integer (most significant bit first, `bytes` MSB-first within each byte).
fn read_bits_be(bytes: &[u8], start: usize, n: usize) -> u64 {
    let mut value = 0u64;
    for i in 0..n {
        let bit = bytes[(start + i) / 8] & (0x80 >> ((start + i) % 8)) != 0;
        value = (value << 1) | u64::from(bit);
    }
    value
}

/// Reads `n` bits (`n <= 64`) starting at bit `start` as a **little-endian** unsigned
/// integer: the bits are gathered MSB-first into `ceil(n / 8)` bytes, and those whole
/// bytes are then interpreted least-significant byte first — the exact inverse of
/// [`BitBuf::push_integer_bits`] with `little = true`.
///
/// A width that is not a whole number of bytes leaves a final **partial** byte, and its
/// `n % 8` bits are the value's next byte (they were pushed into the *high* bits of the
/// last byte), so they enter the value *above* the whole bytes rather than through the
/// byte reversal: `<<212,14:4>>` read as `<<A:12/little>>` is `16#0E bsl 8 bor 16#D4` =
/// 3796, and the byte-aligned `<<0,1>>` as `<<A:16/little>>` is still 256. A width of at
/// most 8 bits is unaffected by `little` (its single partial field *is* the whole value),
/// exactly as the oracle's own reader treats it.
fn read_bits_le(bytes: &[u8], start: usize, n: usize) -> u64 {
    // The byte-wise reader itself lives in the `bits_match` module, next to the oracle
    // table that pins it; this entry point stays the one the integer pattern path calls.
    bits_match::read_little_endian_bits(bytes, start, n)
}

/// Sign-extends the low `n` bits (`n <= 64`) of `value`.
fn sign_extend(value: u64, n: usize) -> i64 {
    if n >= 64 {
        return value as i64;
    }
    let shift = 64 - n;
    ((value << shift) as i64) >> shift
}

/// The low `n` bits set (`n <= 64`).
fn mask_bits(n: usize) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Drops leading zero `ErtsDigit`s (`digits` is least significant first), yielding a
/// normalised magnitude: `[0, 1]` becomes `[1]`, and `[0, 0]` becomes `[0]`.
///
/// Erlang forbids a bignum with a zero-width magnitude (the sign lives in the header), so
/// the one caller trims before [`beam_codec::term::make_big`]; it never yields an empty
/// slice (at least the least significant digit is kept).
fn trim_digits(digits: &[u64]) -> &[u64] {
    let mut end = digits.len();
    while end > 1 && digits[end - 1] == 0 {
        end -= 1;
    }
    &digits[..end]
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
    use beam_term::{atom_val, is_immediate, is_small, signed_value};

    use super::*;
    use crate::config::RuntimeConfig;
    use crate::runtime::Runtime;

    /// Evaluates `expr` (with no program output) and returns the term.
    ///
    /// The runtime (and its heap) is dropped when this returns, so the result is
    /// only usable while it is a **fixed immediate** (a small integer or an atom).
    /// A heap-allocated result (a float, list, tuple, binary, …) would dangle;
    /// such tests drive `RuntimeContext` directly so the runtime outlives the term.
    ///
    /// [`is_immediate`] enforces that contract instead of relying on the comment: a
    /// heap-allocated result would otherwise survive as a dangling pointer and become an
    /// intermittent SIGSEGV in a later read (which is exactly how it presented).
    fn run(expr: &str) -> Result<Eterm> {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, expr)?;
        assert!(
            is_immediate(value),
            "{expr}: a heap-allocated result cannot outlive this helper's runtime (it is \
             dropped on return) — evaluate it through a local `RuntimeContext` instead"
        );
        Ok(value)
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
    fn a_quoted_keyword_atom_evaluates_as_an_atom() {
        // Every expectation below is the system Erlang oracle's own stdout for the same
        // expression: a quoted atom whose text spells an Erlang keyword is an ordinary atom,
        // never the reserved word (`erl_scan:string("'case'")` → `{atom,1,'case'}`), in every
        // position — and a quoted atom is never a keyword terminator or an infix operator.
        assert_eq!(printed(r#"io:format("~p", ['when'])"#), "'when'");
        assert_eq!(printed(r#"io:format("~p", [{'when','of'}])"#), "{'when','of'}");
        assert_eq!(printed(r#"io:format("~p", [#{'case' => 1}])"#), "#{'case' => 1}");
        assert_eq!(printed(r#"io:format("~p", [is_atom('case')])"#), "true");
        assert_eq!(
            printed(r#"io:format("~p", [atom_to_list('receive')])"#),
            "\"receive\""
        );
        assert_eq!(
            printed(r#"io:format("~p", [map_get('when', #{'when' => 1})])"#),
            "1"
        );
        assert_eq!(
            printed(r#"io:format("~p", [[V || V <- ['when','of']]])"#),
            "['when','of']"
        );
        assert_eq!(
            printed(r#"io:format("~p", [try throw('when') catch 'when' -> caught end])"#),
            "caught"
        );
        assert_eq!(
            printed(r#"io:format("~p", [fun('when') -> 'of' end('when')])"#),
            "'of'"
        );
        assert_eq!(printed(r#"io:format("~p", ['when' = 'when'])"#), "'when'");
        // `'end'`/`'of'` do not close a `case`/`begin`: they are plain terms there.
        assert_eq!(
            printed(r#"io:format("~p", [case 'of' of 'end' -> 1; _ -> other end])"#),
            "other"
        );
        assert_eq!(printed(r#"io:format("~p", [begin 'end' end])"#), "'end'");
        // A quoted call head is a function call: `'abs'(-1)` is `erlang:abs(-1)`.
        assert_eq!(printed(r#"io:format("~p", ['abs'(-1)])"#), "1");
        assert_eq!(
            printed(r#"io:format("~p", ['lists':reverse([1,2])])"#),
            "[2,1]"
        );
    }

    #[test]
    fn bit_operators_go_through_the_registry() {
        // The six bit operators are dispatched to `beam_bifs`' arbitrary-precision
        // `erlang` BIFs (not computed locally), so a small-int result must still be
        // exactly the oracle's.
        int("5 band 3", 1);
        int("5 bor 2", 7);
        int("5 bxor 1", 4);
        int("1 bsl 4", 16);
        int("16 bsr 4", 1);
        int("bnot 0", -1);
        // Two's-complement semantics for a negative operand (`utils.c`/`erl_arith.c`).
        int("bnot 5", -6);
        int("-2 band 3", 2);
        // A negative shift amount flips the direction (`erts_shift`).
        int("1 bsl -4", 0);
    }

    #[test]
    fn bit_operator_bignum_round_trip() {
        // `1 bsl 70` is a bignum — only the registry's arbitrary-precision arithmetic
        // produces it (the old local small-int path raised `system_limit`). Shifting
        // back lands on the small integer `1`, so the intermediate bignum term must
        // have survived the round trip through the BIF registry.
        int("(1 bsl 70) bsr 70", 1);
        // A bignum `band` a bignum stays exact and fits a small integer here.
        int("((1 bsl 70) bor 3) band 3", 3);
    }

    /// Evaluates `expr` to an integer and returns its `(negative, magnitude)` limbs
    /// (`ErtsDigit`s, least significant first).
    ///
    /// A bignum is heap-allocated, so it cannot outlive the runtime (see [`run`]); copying
    /// its limbs out is the representation-independent way to assert one.
    fn integer_parts(expr: &str) -> (bool, Vec<u64>) {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        if is_small(value) {
            let value = signed_value(value);
            return (value < 0, vec![value.unsigned_abs()]);
        }
        let (negative, digits) = beam_codec::term::big_parts(value)
            .unwrap_or_else(|_| panic!("{expr} is not an integer"));
        (negative, digits.to_vec())
    }

    #[test]
    fn integer_literals_past_the_small_range_materialize_a_bignum() {
        // 2^63 - 1 is past the 60-bit small range, so the oracle materializes the exact
        // `SMALL_BIG_EXT` bignum (`<<131,110,8,0,255,255,255,255,255,255,255,127>>`). The
        // literal path must build it rather than raise `system_limit`.
        assert_eq!(
            integer_parts("9223372036854775807"),
            (false, vec![0x7fff_ffff_ffff_ffff])
        );
        // 2^63 does not fit `i64`: the lexer keeps an `i128` so the value stays exact.
        assert_eq!(
            integer_parts("9223372036854775808"),
            (false, vec![0x8000_0000_0000_0000])
        );
        // Past 64 bits the magnitude needs two limbs (2^64 + 1).
        assert_eq!(integer_parts("18446744073709551617"), (false, vec![1, 1]));
        // A negative literal keeps the sign in the header and the magnitude in the limbs.
        assert_eq!(
            integer_parts("(-9223372036854775808)"),
            (true, vec![0x8000_0000_0000_0000])
        );
        // A literal still inside the small range stays a small integer.
        int("9223372036854775807 - 9223372036854775806", 1);
    }

    #[test]
    fn an_integer_literal_past_i128_is_a_typed_gap() {
        // 10^40 is past `i128` (~1.7e38): the parser reports the honest capability gap
        // rather than fabricating a truncated value.
        let error = run("10000000000000000000000000000000000000000").expect_err("past i128");
        assert!(error::unsupported_reason(&error).is_some(), "{error}");
    }

    #[test]
    fn improper_list_literals_keep_their_tail() {
        // `[1,2|3]` is a *multi-element* improper list; before the parser accepted a tail
        // after the elements it was a capability gap. The printed term is the oracle's.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        for (expr, expected) in [
            ("[1,2|3]", "[1,2|3]"),
            ("[1|2]", "[1|2]"),
            ("[1,2,3]", "[1,2,3]"),
            ("[1,2|[3]]", "[1,2,3]"),
        ] {
            let value = eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr}: {error}"));
            let printed = ctx.runtime.format_term(value, None).expect("~p renders it");
            assert_eq!(printed.trim_end(), expected, "{expr}");
        }
    }

    #[test]
    fn integer_segments_are_arbitrary_precision() {
        // `<<0:2048>>` is 256 zero bytes — a segment far wider than a machine word (this
        // used to be a typed capability gap).
        assert_eq!(bits("<<0:2048>>"), (vec![0u8; 256], 0, 2048));
        // A negative value is laid out in two's complement within the segment width.
        assert_eq!(bits("<<-1:2048>>"), (vec![0xffu8; 256], 0, 2048));
        // A bignum value truncates to the segment width (`<<(1 bsl 70):8>>` is `<<0>>`).
        assert_eq!(bits("<<(1 bsl 70):8>>"), (vec![0u8], 0, 8));
        // A 72-bit segment lays the bignum's low 72 bits out big-endian (2^64 is bit 64).
        assert_eq!(bits("<<(1 bsl 64):72>>"), (vec![1, 0, 0, 0, 0, 0, 0, 0, 0], 0, 72));
        // … and little-endian reverses the byte order without touching the bit count.
        assert_eq!(
            bits("<<(1 bsl 64):72/little>>"),
            (vec![0, 0, 0, 0, 0, 0, 0, 0, 1], 0, 72)
        );
        // A non-byte-aligned wide segment still ends up a bitstring.
        assert_eq!(bits("<<1:65>>"), (vec![0, 0, 0, 0, 0, 0, 0, 0, 0x80], 0, 65));
    }

    #[test]
    fn bit_operator_on_a_non_integer_is_badarith() {
        // The registry owns the error reason: a float operand is `badarith`
        // (`1.5 band 2`), exactly as the oracle raises it.
        assert!(run("1.5 band 2").is_err(), "1.5 band 2 must fail");
        assert!(run("bnot 1.0").is_err(), "bnot 1.0 must fail");
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
        // The result is heap-allocated, so the runtime must outlive it — drive the
        // `RuntimeContext` directly (see `run`) rather than letting it escape `run`, which
        // drops its runtime (and heap) before returning.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let concatenated = eval(&mut ctx, "[1,2] ++ [3]").expect("[1,2] ++ [3]");
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
    fn an_exported_fun_is_the_real_export_ext_term() {
        // `fun m:f/A` is not one of this evaluator's closures: it captures no environment,
        // so the term **is** the fun — the real `EXPORT_EXT` encoding. `term_to_binary/1`
        // therefore emits tag 113 + module `lists` + function `reverse` + arity 1, the
        // differential `wire/export_ext_encode` golden (20 bytes).
        assert_eq!(
            bits("term_to_binary(fun lists:reverse/1)"),
            (
                vec![
                    131, 113, 119, 5, 108, 105, 115, 116, 115, 119, 7, 114, 101, 118, 101, 114,
                    115, 101, 97, 1,
                ],
                0,
                20 * 8
            )
        );
        // Applying it stays an ordinary dynamic call, so all three shapes work: a remote
        // fun, the `erlang:apply/2` seam, an auto-imported `erlang` fun and the
        // module-relative `fun Name/Arity` form.
        assert_eq!(ints("(fun lists:reverse/1)([1,2,3])"), vec![3, 2, 1]);
        assert_eq!(
            ints("erlang:apply(fun lists:reverse/1, [[1,2,3]])"),
            vec![3, 2, 1]
        );
        assert_eq!(ints("lists:map(fun erlang:abs/1, [-1,2])"), vec![1, 2]);
        assert_eq!(ints("lists:map(fun length/1, [[1],[2,3]])"), vec![1, 2]);
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
        // `erlang:statistics(active_tasks)` is a valid statistics key this runtime
        // cannot measure (it needs per-scheduler task accounting), so the `RuntimeHooks`
        // bridge reports it as the `unsupported` capability gap. A `try`/`catch` in the
        // expression must NOT turn that into an apparent exception: it propagates out
        // (classified by `unsupported_reason`) so the CLI exits 3 with
        // `beam_rt: unsupported: …`. (The other measurable keys — `statistics(runtime)`
        // from the runtime's CPU-time clock, alongside `statistics(garbage_collection)`/
        // `statistics(wall_clock)`/`statistics(run_queue)`/`statistics(reductions)`/
        // `statistics(scheduler_wall_time)` — are **served** and cross-checked against the
        // oracle by `tests/runtime_hooks.rs`.)
        for expr in [
            "try erlang:statistics(active_tasks) catch _:_ -> 0 end",
            "try (erlang:statistics(active_tasks)) of V -> V catch C:R -> ok end",
        ] {
            let error = run(expr).expect_err("a capability gap propagates");
            assert!(
                error::unsupported_reason(&error).is_some(),
                "{expr}: {error}"
            );
        }

        // `erlang:send/2` by contrast is **served** now that the runtime installs the
        // delivery hook (`context::bridges::SendBridge`), so it is an ordinary, catchable
        // send: `erlang:send(self(), hello), receive X -> X end` ⇒ `hello`.
        atom(
            "begin erlang:send(self(), hello), receive X -> X end end",
            "hello",
        );
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

    /// The `{case_clause, V}`/`if_clause` raises the evaluator itself produces are converted
    /// to a [`BeamError`] by [`finish`] — the module's **only** terminal conversion of a
    /// [`Flow::Raise`] (both call sites, [`eval`] and [`apply_eval_closure`], go through it).
    ///
    /// A **structured** reason (`{case_clause, Subject}`, …) is a term the term-free
    /// [`ExitReason`](beam_common::ExitReason) cannot hold, so [`finish`] attaches it to the
    /// raise's live-reason slot: [`crate::error::live_reason_term`] — the runtime's single
    /// recovery point, and what every rendering route consumes — must find it. Before the
    /// slot was attached the reason rendered as the `<structured reason>` placeholder.
    ///
    /// Every expectation is the system oracle's own `~w` text for that reason, verified live
    /// by [`the_uncaught_reason_text_is_the_oracles`] below.
    #[test]
    fn an_uncaught_structured_reason_carries_its_live_term() {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        for (expr, expected) in [
            ("case 1 of 2 -> ok end", "{case_clause,1}"),
            ("case [a,b,c] of [x] -> y end", "{case_clause,[a,b,c]}"),
            ("case <<1,2>> of <<3,4>> -> ok end", "{case_clause,<<1,2>>}"),
        ] {
            let error = eval(&mut ctx, expr).expect_err(expr);
            let BeamError::Raise(raise) = &error else {
                panic!("{expr}: expected a raise, got {error:?}");
            };
            assert_eq!(raise.class, ExceptionClass::Error, "{expr}");
            assert_eq!(
                raise.reason.as_atom(),
                None,
                "{expr}: a structured reason names no atom"
            );
            let reason = error::live_reason_term(&error)
                .unwrap_or_else(|| panic!("{expr}: the reason term must be live"));
            let printed = ctx
                .runtime
                .format_term_w(reason, None)
                .expect("~w renders it");
            assert_eq!(printed.trim_end(), expected, "{expr}");
        }
    }

    /// An evaluator-produced **atom** reason is unchanged: the atom is the authoritative
    /// reason, so [`finish`] attaches no term next to it and
    /// [`crate::error::live_reason_term`] keeps answering `None` (an atom reason must never be
    /// shadowed by a term — see its own doc).
    ///
    /// `if` with no matching clause raises the **bare atom** `if_clause` (the oracle prints
    /// `exc error if_clause`), which is an evaluator raise with `origin: None` — i.e. exactly
    /// the arm [`finish`] rewrites.
    #[test]
    fn an_uncaught_atom_reason_is_not_shadowed_by_a_live_term() {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let error = eval(&mut ctx, "if false -> ok end").expect_err("an unmet `if`");
        let BeamError::Raise(raise) = &error else {
            panic!("expected a raise, got {error:?}");
        };
        assert_eq!(raise.class, ExceptionClass::Error);
        // The atom is still the reason, and it still *names* the reason.
        assert_eq!(raise.reason.as_atom(), Some("if_clause"));
        assert_eq!(raise.reason.to_string(), "if_clause");
        assert!(
            raise.live_reason().is_none(),
            "an atom reason must not carry a live term"
        );
        assert_eq!(error::live_reason_term(&error), None);
    }

    /// The system `erl`'s stdout for `expression`, or `None` — with a **loud** skip on stderr
    /// — when no `erl` is on `PATH`, or when the probe outlived `ORACLE_TIMEOUT`.
    ///
    /// The bound is not optional: a bare `erl -noshell` never exits, so every probe here ends
    /// in `halt/0` and a hung `erl` is killed rather than allowed to wedge the suite.
    fn oracle_stdout(expression: &str) -> Option<String> {
        const ORACLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        let mut child = match std::process::Command::new("erl")
            .args(["-noshell", "-eval", expression])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "SKIP: no erl on PATH — the uncaught `-eval` raise reason text is not \
                     cross-checked against the system Erlang"
                );
                return None;
            }
            Err(error) => panic!("running the erl oracle: {error}"),
        };
        let deadline = std::time::Instant::now() + ORACLE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!(
                        "SKIP: the erl probe did not exit within {ORACLE_TIMEOUT:?} — the \
                         uncaught `-eval` raise reason text is not cross-checked"
                    );
                    return None;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(error) => panic!("polling the erl oracle: {error}"),
            }
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).expect("oracle stdout");
        Some(text)
    }

    /// The reason **text** an uncaught `-eval` raise renders is the oracle's own.
    ///
    /// Each expression is run under the system `erl` inside the mandatory self-terminating
    /// wrapper, so the oracle prints `exc error <Reason>` (its `~w` rendering of the reason)
    /// and exits 1; this runtime must render the identical `~w` text from its raise — through
    /// [`crate::error::live_reason_term`] for a structured reason and through the atom itself
    /// otherwise, which is exactly what `cli.rs`'s `reported_reason` does for a route that
    /// consumes the slot. This is the live cross-check that keeps the hardcoded
    /// expectations above honest.
    #[test]
    fn the_uncaught_reason_text_is_the_oracles() {
        // The four reasons below in one `erl` invocation (the oracle is spawned once, not
        // four times): `W/1` is the mandatory wrapper, applied to each expression in turn.
        const PROBE: &str = r#"W = fun(E) -> try (fun() -> E() end)() of V -> io:format("ok ~w~n", [V]), ok catch C:R -> io:format("exc ~w ~w~n", [C, R]), ok end end, [W(E) || E <- [fun() -> case 1 of 2 -> ok end end, fun() -> case [a,b,c] of [x] -> y end end, fun() -> case <<1,2>> of <<3,4>> -> ok end end, fun() -> if false -> ok end end]], halt(0)."#;
        let Some(stdout) = oracle_stdout(PROBE) else {
            return;
        };
        let cases = [
            ("case 1 of 2 -> ok end", "{case_clause,1}"),
            ("case [a,b,c] of [x] -> y end", "{case_clause,[a,b,c]}"),
            ("case <<1,2>> of <<3,4>> -> ok end", "{case_clause,<<1,2>>}"),
            ("if false -> ok end", "if_clause"),
        ];
        let oracle_lines: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            oracle_lines.len(),
            cases.len(),
            "the oracle probe printed {oracle_lines:?}"
        );

        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        for (index, (expr, expected)) in cases.iter().enumerate() {
            assert_eq!(
                oracle_lines[index],
                format!("exc error {expected}"),
                "{expr} under the oracle"
            );
            let error = eval(&mut ctx, expr).expect_err(expr);
            let rendered = match error::live_reason_term(&error) {
                Some(term) => ctx
                    .runtime
                    .format_term_w(term, None)
                    .expect("~w renders the reason")
                    .trim_end()
                    .to_string(),
                None => {
                    let BeamError::Raise(raise) = &error else {
                        panic!("{expr}: expected a raise, got {error:?}");
                    };
                    raise.reason.to_string()
                }
            };
            assert_eq!(
                format!("exc error {rendered}"),
                oracle_lines[index],
                "{expr}: this runtime's reason text"
            );
        }
    }

    /// `erlang:raise/3` is **not** served by the evaluator: it falls through to the
    /// registered `beam_bifs::convert::raise_3` BIF, so its class and its stacktrace
    /// argument are validated exactly as the oracle validates them and the **decoded
    /// trace rides the raise**.
    ///
    /// The invalid shapes are the *returned* atom `badarg` (the C's
    /// `BIF_RET(am_badarg)`) — a **value**, not a raise — so `try erlang:raise(foo, foo,
    /// []) catch C:R -> {C,R} end` never runs its catch. Every expectation is the system
    /// oracle's (`erl -noshell -eval …`, OTP 29 / erts-17.1), captured through the
    /// corpus's own probe wrapper in `../../tests/eval.rs`.
    #[test]
    fn raise_is_served_by_the_registry_bif_carrying_the_decoded_stacktrace() {
        // An invalid class (`foo`, `1`) or an invalid stacktrace shape (`42`, `x`, `{}`)
        // is returned, never raised: the call evaluates to the atom `badarg`.
        atom("erlang:raise(foo, foo, [])", "badarg");
        atom("erlang:raise(1, foo, [])", "badarg");
        atom("erlang:raise(error, foo, [42])", "badarg");
        atom("erlang:raise(error, foo, x)", "badarg");
        atom("erlang:raise(error, foo, [{}])", "badarg");

        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);

        // A valid call raises, and the raise **carries the decoded trace** — the frames
        // the oracle echoes (`{m,f,1}` ⇒ `m:f/1`; the `Location` form keeps its
        // `file`/`line`). The raise must survive `flow_from_error` → `finish` unchanged.
        let error = eval(&mut ctx, "erlang:raise(error, foo, [{m,f,1}])")
            .expect_err("a valid raise propagates out of the evaluation");
        let BeamError::Raise(raise) = &error else {
            panic!("expected a raise, got {error:?}");
        };
        assert_eq!(raise.class, ExceptionClass::Error);
        assert_eq!(raise.reason.as_atom(), Some("foo"));
        assert_eq!(
            raise.stacktrace.to_string(),
            "m:f/1",
            "the decoded stacktrace must ride the returned raise"
        );

        let error = eval(
            &mut ctx,
            r#"erlang:raise(exit, {shutdown,1}, [{lists,map,2,[{file,"lists.erl"},{line,1234}]}])"#,
        )
        .expect_err("a valid raise propagates out of the evaluation");
        let BeamError::Raise(raise) = &error else {
            panic!("expected a raise, got {error:?}");
        };
        assert_eq!(raise.class, ExceptionClass::Exit);
        assert_eq!(
            raise.stacktrace.to_string(),
            "lists:map/2 (lists.erl:1234)"
        );
        // A non-atom reason is the live argument term (the raise carries no atom to
        // shadow it): `{shutdown,1}` must come back exactly.
        let reason = error::live_reason_term(&error).expect("the reason term is live");
        let printed = ctx
            .runtime
            .format_term(reason, None)
            .expect("~p renders it");
        assert_eq!(printed.trim_end(), "{shutdown,1}");

        // The empty list is a **valid** trace: it decodes to the empty stacktrace, so
        // `raise/3` keeps its class and reason and derives nothing.
        let error = eval(&mut ctx, "erlang:raise(throw, x, [])")
            .expect_err("a valid raise propagates out of the evaluation");
        let BeamError::Raise(raise) = &error else {
            panic!("expected a raise, got {error:?}");
        };
        assert_eq!(raise.class, ExceptionClass::Throw);
        assert_eq!(raise.reason.as_atom(), Some("x"));
        assert!(raise.stacktrace.is_empty(), "`[]` decodes to an empty trace");
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
        // `self() ! hello` is **implemented**: the send reaches the running process's own
        // mailbox, so the expression's value is the message term — the atom `hello`, exactly
        // as the oracle's `self() ! hello` returns it.
        atom("self() ! hello", "hello");
        // A bare `receive` with nothing runnable to deliver a message is still a genuine
        // capability gap (there is no host timer to block on in `-eval`), never a wrong
        // result.
        let error = run("receive X -> X end").expect_err("a bare receive is a gap");
        assert!(
            error::unsupported_reason(&error).is_some(),
            "receive X -> X end: {error}"
        );
    }

    /// Evaluates `expr` to a bitstring and copies out its `(bytes, offset_bits,
    /// size_bits)`.
    ///
    /// A bitstring result is heap-allocated, so it cannot outlive the runtime (see
    /// [`run`]); copying the bytes out is the printer-independent way to assert one.
    /// The `~w`/`~p` printer rejects a **non-byte-aligned** bitstring (a `beam_bifs` gap
    /// — see the crate `CONTEXT.md`), so printing could not be used here.
    fn bits(expr: &str) -> (Vec<u8>, usize, usize) {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        let (bytes, offset_bits, size_bits) = beam_codec::term::bitstring_bytes(value)
            .unwrap_or_else(|_| panic!("{expr} did not produce a bitstring"));
        (bytes.to_vec(), offset_bits, size_bits)
    }

    /// Evaluates `expr` to a list and returns its small-integer elements.
    fn ints(expr: &str) -> Vec<i64> {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        terms::list_elements(value)
            .unwrap_or_else(|_| panic!("{expr} did not produce a list"))
            .iter()
            .map(|element| signed_value(*element))
            .collect()
    }

    /// Evaluates `expr` with `io:format/2` captured and returns exactly what it printed.
    ///
    /// A construct's **evaluation order** is observable only through a side effect, so the
    /// order probes below hand each sub-expression to a "spy" closure that prints it; the
    /// captured bytes *are* the order the sub-expressions ran in.
    fn printed(expr: &str) -> String {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::BytesSink::new();
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        drop(ctx);
        String::from_utf8_lossy(sink.contents()).into_owned()
    }

    /// A literal evaluates its sub-expressions **left-to-right in source order**, exactly as
    /// `erl_eval` does: a list's elements before its tail, a map field's key before its
    /// value, a binary segment's value before its size, call arguments in order.
    ///
    /// Every expectation below is the exact stdout the system Erlang oracle prints for the
    /// same expression (`erl -noshell -eval '<expr>'`), so it is a real cross-check and not
    /// just this runtime's own habit. The list cases are the regression: `Evaluator::list`
    /// used to evaluate the elements right-to-left *and* the tail first, printing `321`/`312`
    /// where the oracle prints `123`/`1234`.
    #[test]
    fn literals_evaluate_their_sub_expressions_in_source_order() {
        const SPY: &str = r#"F = fun(X) -> io:format("~w", [X]), X end"#;
        // `<spy>, _ = <body>, ok` — the body's sub-expressions print, in the order they run.
        // The space around `=` avoids the `=<` token when `body` starts with `<<`.
        let probe = |body: &str| format!("{SPY}, _ = {body}, ok");

        // A list's elements are evaluated left-to-right, then its tail.
        assert_eq!(printed(&probe("[F(1),F(2),F(3)]")), "123");
        assert_eq!(printed(&probe("[F(1),F(2),F(3)|F(4)]")), "1234");
        assert_eq!(printed(&probe("[F(1)|F(2)]")), "12");
        // Only *when* the sub-expressions run changed — the list's value is untouched: the
        // elements print `.<1>.<2>.<3>` and the finished list is `~w`-printed as `=[1,2|3]`.
        assert_eq!(
            printed(
                r#"F = fun(X) -> io:format(".<~w>", [X]), X end, V = [F(1),F(2)|F(3)], io:format("=~w", [V]), ok"#
            ),
            ".<1>.<2>.<3>=[1,2|3]"
        );

        // A tuple's elements, in order.
        assert_eq!(printed(&probe("{F(1),F(2),F(3)}")), "123");
        // A map's fields in source order, each field's key before its value.
        assert_eq!(printed(&probe("#{F(1)=>F(2), F(3)=>F(4)}")), "1234");
        assert_eq!(
            printed(&probe("#{F(1)=>F(2), F(3)=>F(4), F(5)=>F(6)}")),
            "123456"
        );
        // A binary's segments in source order, and a segment's value before its size.
        assert_eq!(printed(&probe("<<(F(1)):8,(F(2)):8>>")), "12");
        assert_eq!(printed(&probe("<<(F(7)):(F(8))>>")), "78");
        // A call's arguments, left-to-right; the spy callee prints its own arguments too.
        assert_eq!(
            printed(&format!(
                r#"{SPY}, G = fun(A,B) -> io:format("~w~w", [A,B]), ok end, _ = G(F(1),F(2)), ok"#
            )),
            "1212"
        );
        // `++`/`--` evaluate their left operand before their right one.
        assert_eq!(printed(&probe("[F(1)] ++ [F(2)]")), "12");
        assert_eq!(printed(&probe("[F(1),F(2)] -- [F(3)]")), "123");
        // A comprehension's template, once per generated element in generator order.
        assert_eq!(printed(&probe("[F(X) || X <- [1,2,3]]")), "123");
    }
    #[test]
    fn binary_comprehensions_iterate_a_segment_at_a_time() {
        // The differential `semantics/binary_comprehension` probe:
        // `<< <<(X+1)>> || <<X>> <= <<1,2,3>> >>` is `<<2,3,4>>`.
        assert_eq!(
            bits("<< <<(X+1)>> || <<X>> <= <<1,2,3>> >>"),
            (vec![2, 3, 4], 0, 24)
        );
        // A binary generator drives a *list* comprehension just as well.
        assert_eq!(ints("[X || <<X>> <= <<1,2,3>>]"), vec![1, 2, 3]);
        // A sized pattern consumes a fixed width per iteration: `<<0,1,0,2>>` yields the
        // two 16-bit big-endian integers (the `semantics/…` sized-pattern form).
        assert_eq!(ints("[X || <<X:16>> <= <<0,1,0,2>>]"), vec![1, 2]);
        // A filter still applies to a binary generator.
        assert_eq!(ints("[X || <<X>> <= <<1,2,3,4>>, X > 2]"), vec![3, 4]);
    }

    #[test]
    fn non_byte_aligned_segments_materialize_a_bitstring() {
        // The differential `semantics/bit_size_of_bitstring` probe.
        int("bit_size(<<1:1>>)", 1);
        assert_eq!(bits("<<1:1>>"), (vec![0x80], 0, 1));

        // 12 bits do not fill a byte: `<<255:12>>` is the oracle's `<<15,15:4>>`.
        int("bit_size(<<255:12>>)", 12);
        assert_eq!(bits("<<255:12>>"), (vec![0x0F, 0xF0], 0, 12));

        // `little` lays the value's bytes out least-significant first before the value
        // is truncated to 12 bits: `<<255:12/little>>` is the oracle's `<<255,0:4>>`.
        int("bit_size(<<255:12/little>>)", 12);
        assert_eq!(bits("<<255:12/little>>"), (vec![0xFF, 0x00], 0, 12));
    }

    #[test]
    fn adjacent_segments_share_one_bit_stream() {
        // `1:1` then `2:7` fill exactly one byte (130).
        assert_eq!(
            bits("begin A = 1, B = 2, <<A:1,B:7>> end"),
            (vec![130], 0, 8)
        );
        // A sized integer head leaves a bit offset that a following `binary` tail keeps:
        // `5:3` then the byte 2 is the oracle's `<<160,2:3>>`.
        assert_eq!(
            bits("begin A = 5, N = 3, Rest = <<2>>, <<A:N, Rest/binary>> end"),
            (vec![0xA0, 0x40], 0, 11)
        );
        // A sized `bits` head takes the first `N` bits of a bitstring value: `<<1:1>>`
        // then the byte 2 is the oracle's `<<129,0:1>>`.
        assert_eq!(
            bits("begin A = <<1:1>>, N = 1, Rest = <<2>>, <<A:N/bits, Rest/binary>> end"),
            (vec![0x81, 0x00], 0, 9)
        );
    }
}
