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

//! The baseline JIT's comparison fast paths: the eight `erlang` comparison BIFs
//! (`=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`) lowered natively for small-int and
//! immediate operands (the section comment below proves each rule is
//! interpreter-identical).
//!
//! Like the guarded small-int arithmetic fast path ([`super::arith`]) this is a
//! **shape** test (never a value test): a refused shape, and every run-time guard
//! rejection, deopt to exactly one `jit_step_one` — the instruction's exact
//! interpreter semantics.

use beam_common::Result;
use beam_loader::{Instr, Module, Operand};
use beam_term::{make_atom, NIL};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::arith::{import_names, FAST_A, FAST_B, FAST_C, FAST_T, SMALL_MASK};
use super::operands::{small_word, constant_word, x_offset, x_register};
use super::{emit_step_and_continue, touch, Allocation};

// ---------------------------------------------------------------------------
// The comparison fast paths
// ---------------------------------------------------------------------------
//
// The eight comparison operators are the compiler's clause guards and boolean
// bodies (the settled corpus emits every one of them as a **non-collecting**
// `{bif,Op,{f,0},Args,Dst}` with `Fail = 0`, never as a `gc_bif`), so leaving them
// to `jit_step_one` costs a helper round-trip per comparison. They are total
// functions — `beam_bifs` returns `am_true`/`am_false` for any two terms
// (`erts/emulator/beam/erl_bif_op.c:103-131`) — so the fast path below produces a
// boolean *without* consulting `ctx.call_bif`, and any shape it cannot prove is
// exact deopts to the interpreter step (which reproduces the fail/raise split,
// `Fail = 0` included).
//
// Each operator is lowered as `load a; load b; guard(s); cmp a, b; jcc(cond,
// true); false; true`. The rules, and why each is the interpreter's own result
// (`cmp` / `cmp_arith` over the module's atom table):
//
// * `=:=`/`=/=` use the **exact** order (`cmp`). Two registers are guarded as
//   smalls first: `make_small(v) == v*16 + 15` is injective and `cmp`'s
//   `a.raw() == b.raw()` fast path is exact, so a plain word compare of two small
//   words is exactly `=:=`. With at least one **constant** operand a word compare
//   is already exact (an immediate atom/`nil` word has low bits `0xB`/`0x3B` and a
//   small word has low nibble `0xF`, while a boxed pointer is even — no cross-class
//   collision — and `cmp`'s exact mode never equates two different classes), which
//   is the shape [`super::test`] already inlines for `is_eq_exact`/`is_ne_exact`.
// * `==`/`/=` use the **arithmetic** order (`cmp_arith`). It is the one comparison
//   where an immediate is not self-sufficient (`1 == 1.0`, `-0.0 == +0.0`), so a
//   small-int operand's *register* counterpart is guarded as a small. An atom/`nil`
//   constant is `==` only to the identical word, so it needs no guard — the
//   conservative `Guard::None` rule [`super::test::arith_guard`] uses.
// * `<`/`=<`/`>`/`>=` use the **arithmetic order** too, and only two *numbers* are
//   word-order exact: `make_small(v)` is monotonic, so the tagged-word **signed**
//   compare is the value order for two smalls (a guarded register included). An
//   atom/`nil` constant is not word-order exact (`cmp_arith` orders the term
//   classes, not their words), so those shapes stay on the helper.
//
// The result is the module's own `true`/`false` atom word; the whole instruction
// deopts when either name is absent from the module's atom table, so a wrong atom
// is never fabricated. `beam_rt` loads every module into one append-only shared
// atom table, so these are the very words `beam_bifs`' `ctx.boolean(..)` builds —
// the interpreter's result term and the fast path's are the same Eterm.
//
// `>`/`=<` are lowered directly (`Cond::G`/`Cond::Le` over `cmp a, b`), never by
// swapping the operands: `cmp_arith` is **not** antisymmetric for the boxed pairs
// the guard rejects, so a swapped-operand form could flip which operand order a
// deopt would have compared.

/// The eight `erlang` comparison operators, split by the order they use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ComparisonOp {
    /// `erlang:'=:='/2` — the exact order.
    ExactEq,
    /// `erlang:'=/='/2` — the exact order.
    ExactNe,
    /// `erlang:'=='/2` — the arithmetic order.
    ArithEq,
    /// `erlang:'/='/2` — the arithmetic order.
    ArithNe,
    /// `erlang:'<'/2` — the arithmetic order.
    Lt,
    /// `erlang:'=<'/2` — the arithmetic order.
    Le,
    /// `erlang:'>'/2` — the arithmetic order.
    Gt,
    /// `erlang:'>='/2` — the arithmetic order.
    Ge,
}

impl ComparisonOp {
    /// The operator `name` denotes, or `None` for any other BIF.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "=:=" => Self::ExactEq,
            "=/=" => Self::ExactNe,
            "==" => Self::ArithEq,
            "/=" => Self::ArithNe,
            "<" => Self::Lt,
            "=<" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            _ => return None,
        })
    }

    /// The condition on `cmp a, b` (the two loaded operand words, in instruction
    /// order) that means the operator **succeeded**.
    fn cond(self) -> Cond {
        match self {
            Self::ExactEq | Self::ArithEq => Cond::E,
            Self::ExactNe | Self::ArithNe => Cond::Ne,
            Self::Lt => Cond::L,
            Self::Le => Cond::Le,
            Self::Gt => Cond::G,
            Self::Ge => Cond::Ge,
        }
    }

    /// Whether the operator uses the exact (`cmp`) rather than arithmetic
    /// (`cmp_arith`) order.
    fn is_exact(self) -> bool {
        matches!(self, Self::ExactEq | Self::ExactNe)
    }

    /// Whether the operator is an ordering (`<`/`=<`/`>`/`>=`).
    fn is_order(self) -> bool {
        matches!(self, Self::Lt | Self::Le | Self::Gt | Self::Ge)
    }
}

/// Which operands need the small-int guard for the word compare to reproduce the
/// interpreter exactly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// No guard: the comparison is already exact over the loaded words.
    None,
    /// Guard the first operand.
    Lhs,
    /// Guard the second operand.
    Rhs,
    /// Guard both operands.
    Both,
}

/// One comparison operand the tier can read natively. The payload is only ever
/// matched by variant (the guard rules need the *class*, not the value), so the
/// fields are deliberately unread.
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum CmpOperand {
    /// An in-bounds x-register ([`MAX_REGISTER`]).
    Reg(u32),
    /// A small-int constant's raw tagged word (`{integer,V}`/`{char,C}`).
    Small(i64),
    /// A non-numeric immediate constant (`{atom,A}`/`nil`).
    Immediate(i64),
}

impl CmpOperand {
    /// Whether the operand is a constant (never a runtime value).
    fn is_const(self) -> bool {
        matches!(self, Self::Small(_) | Self::Immediate(_))
    }
}

/// A comparison-BIF shape the fast path targets — a *purely syntactic* check that
/// never consults the register allocation, so [`super::regalloc::allocate_registers`]
/// and [`super::emit_block`] agree on which instructions the fast path lowers.
pub(crate) struct Comparison<'a> {
    /// Which operator is called.
    op: ComparisonOp,
    /// The first operand.
    a: &'a Operand,
    /// The second operand.
    b: &'a Operand,
    /// The operands the small-int guard covers.
    guard: Guard,
    /// The x-register index the result is written to.
    dst_x: u32,
}

impl Comparison<'_> {
    /// The x-register indices the shape needs cached, in load order (the operands
    /// first, then the destination) — exactly the order
    /// [`super::regalloc::allocate_registers`] caches them in.
    pub(super) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(3);
        for operand in [self.a, self.b] {
            if let Some(x) = x_register(operand) {
                registers.push(x);
            }
        }
        registers.push(self.dst_x);
        registers
    }

    /// The x-register index the shape's native lowering **writes** — the destination
    /// (`true`/`false`). The two operands are only read.
    ///
    /// This is the block register cache's write set ([`Allocation::written`]).
    pub(super) fn writes(&self) -> Vec<u32> {
        vec![self.dst_x]
    }
}

/// The shape test for `instr` (`Instr::Bif` or `Instr::GcBif`), or `None` when the
/// instruction is not one of the eight comparison operators over addressable
/// x-registers / immediate constants.
///
/// The `Fail` label is deliberately **not** inspected: the fast path never produces
/// an error and never branches to `fail`, and a guard rejection deopts to the full
/// interpreter step, which reproduces the fail/raise split for a live label and a
/// `NO_FAIL` (`{f,0}`) alike. Requiring a live fail label would inline nothing —
/// the settled corpus emits every comparison with `Fail = 0`.
pub(crate) fn comparison_shape<'a>(module: &Module, instr: &'a Instr) -> Option<Comparison<'a>> {
    let (arity, bif, args, dst) = match instr {
        Instr::Bif {
            arity, bif, args, dst, ..
        } => (*arity, *bif, args, dst),
        Instr::GcBif {
            arity, bif, args, dst, ..
        } => (*arity, *bif, args, dst),
        _ => return None,
    };
    let (module_name, function_name, import_arity) = import_names(module, bif)?;
    if module_name != "erlang" || import_arity != 2 || arity != 2 || args.len() != 2 {
        return None;
    }
    let op = ComparisonOp::from_name(function_name)?;
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER {
        return None;
    }
    let lhs = cmp_operand(&args[0])?;
    let rhs = cmp_operand(&args[1])?;
    let guard = comparison_guard(op, lhs, rhs)?;
    // The result words must be interned: an absent `true`/`false` would have to be
    // fabricated, and the whole instruction stays on the helper instead.
    result_words(module)?;
    Some(Comparison {
        op,
        a: &args[0],
        b: &args[1],
        guard,
        dst_x,
    })
}

/// The raw `Eterm` words of the module's `true` and `false` atoms, or `None` when
/// either is absent from its atom table.
fn result_words(module: &Module) -> Option<(i64, i64)> {
    let truthy = module.atom_table.lookup("true")?;
    let falsy = module.atom_table.lookup("false")?;
    Some((
        make_atom(truthy).raw() as i64,
        make_atom(falsy).raw() as i64,
    ))
}

/// The native source of one operand, or `None` for a `Y` register, a float
/// register, a literal, a bignum constant, or an x-register past [`MAX_REGISTER`].
fn cmp_operand(operand: &Operand) -> Option<CmpOperand> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(CmpOperand::Reg(x)),
        Some(_) => None,
        None => match operand {
            Operand::Integer(value) => small_word(*value).map(CmpOperand::Small),
            Operand::Character(c) => small_word(i64::from(*c)).map(CmpOperand::Small),
            Operand::Atom(atom) => Some(CmpOperand::Immediate(make_atom(*atom).raw() as i64)),
            Operand::Nil => Some(CmpOperand::Immediate(NIL.raw() as i64)),
            _ => None,
        },
    }
}

/// The guard `op` needs over `lhs`/`rhs` for the word compare to be exact, or
/// `None` when the shape is not provably word-exact and must stay on the helper.
fn comparison_guard(op: ComparisonOp, lhs: CmpOperand, rhs: CmpOperand) -> Option<Guard> {
    if op.is_order() {
        // Only two smalls are word-order exact; an atom/`nil` constant (or any
        // bignum/literal operand) keeps the instruction on the helper.
        return Some(match (lhs, rhs) {
            (CmpOperand::Immediate(_), _) | (_, CmpOperand::Immediate(_)) => return None,
            (CmpOperand::Small(_), CmpOperand::Small(_)) => Guard::None,
            (CmpOperand::Small(_), CmpOperand::Reg(_)) => Guard::Rhs,
            (CmpOperand::Reg(_), CmpOperand::Small(_)) => Guard::Lhs,
            (CmpOperand::Reg(_), CmpOperand::Reg(_)) => Guard::Both,
        });
    }
    if op.is_exact() {
        // `=:=`/`=/=`: a word compare is exact when at least one operand is a
        // constant immediate; two registers must both be guarded as smalls.
        return Some(match (lhs, rhs) {
            (CmpOperand::Reg(_), CmpOperand::Reg(_)) => Guard::Both,
            _ if lhs.is_const() || rhs.is_const() => Guard::None,
            _ => return None,
        });
    }
    // `==`/`/=` (`cmp_arith`): an atom/`nil` immediate is `==` only to the identical
    // word, so it needs no guard; a small-int constant's register counterpart does.
    Some(match (lhs, rhs) {
        (CmpOperand::Immediate(_), _) | (_, CmpOperand::Immediate(_)) => Guard::None,
        (CmpOperand::Small(_), CmpOperand::Small(_)) => Guard::None,
        (CmpOperand::Small(_), CmpOperand::Reg(_)) => Guard::Rhs,
        (CmpOperand::Reg(_), CmpOperand::Small(_)) => Guard::Lhs,
        (CmpOperand::Reg(_), CmpOperand::Reg(_)) => Guard::Both,
    })
}

/// Where the fast path reads one operand from.
#[derive(Clone, Copy)]
enum CmpSource {
    /// A cached x-register (already loaded by the prologue).
    Cached(Reg),
    /// A non-cached x-register, read straight from its `machine.x` slot.
    Memory(u32),
    /// A constant's raw `Eterm` word.
    Constant(i64),
}

/// The [`CmpSource`] for `operand`, or `None` when it is not an addressable
/// x-register or an immediate constant (the same set [`cmp_operand`] accepts).
fn cmp_source(operand: &Operand, allocation: &Allocation) -> Option<CmpSource> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(match allocation.slot(x) {
            Some(reg) => CmpSource::Cached(reg),
            None => CmpSource::Memory(x),
        }),
        Some(_) => None,
        None => constant_word(operand).map(CmpSource::Constant),
    }
}

/// Load `source` into scratch register `reg`, recording a `machine.x` read.
fn load_cmp_source(
    buffer: &mut CodeBuffer,
    source: CmpSource,
    reg: Reg,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match source {
        CmpSource::Cached(src) => buffer.mov_rr(reg, src),
        CmpSource::Memory(x) => {
            touch(highest_touched, x);
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
        }
        CmpSource::Constant(word) => buffer.mov_ri(reg, word),
    }
}

/// Branch to `deopt` unless `reg` holds a small integer (`(word & 0xF) == 0xF`).
/// `FAST_C` must already hold [`SMALL_MASK`] (the tag and the mask are the same
/// nibble).
fn guard_small(buffer: &mut CodeBuffer, reg: Reg, deopt: CodeLabel) -> Result<()> {
    buffer.mov_rr(FAST_T, reg)?;
    buffer.and_rr(FAST_T, FAST_C)?;
    buffer.cmp_rr(FAST_T, FAST_C)?;
    buffer.jcc(Cond::Ne, deopt)
}

/// Write the raw `Eterm` word `word` into `dst_x` — its cached machine register,
/// or straight into its `machine.x` slot (the exact slot the exit spill would write).
fn materialize(
    buffer: &mut CodeBuffer,
    word: i64,
    dst_x: u32,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match allocation.slot(dst_x) {
        Some(reg) => buffer.mov_ri(reg, word),
        None => {
            buffer.mov_ri(FAST_T, word)?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), FAST_T)?;
            touch(highest_touched, dst_x);
            Ok(())
        }
    }
}

/// Lower a comparison-BIF shape natively, or report that the block must helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the result in
/// the destination and letting the block continue with the next instruction.
/// Returns `false` **without having emitted a byte** for any shape
/// [`comparison_shape`] refuses (or, defensively, an operand that is not
/// loadable), so the caller emits exactly today's `jit_step_one` fallback.
///
/// The emitted shape (see the section comment above for why each rule is exact):
///
/// ```text
///   mov  rax, a              ; a cached register, `[rbx + x*8]`, or a movabs constant
///   mov  r8,  b
///   mov  rcx, SMALL_MASK     ; only when the guard covers an operand
///   mov  r10, rax ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; a is a small?
///   mov  r10, r8  ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; b is a small?
///   cmp  rax, r8
///   jcc  <cond> true         ; the operator succeeds
///   mov  <dst>, false        ; cached: its machine register; else `[rbx + dst*8]`
///   jmp  fast_done
/// true:
///   mov  <dst>, true
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spill, call `jit_step_one`, continue
///    at `pc + 1` when the helper advanced by one, else exit to the driver>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// Every operand is loaded and every guard applied **before** the destination is
/// written, so a destination aliasing an operand reads the original value (matching
/// the interpreter, which reads all arguments before writing the result).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_comparison(
    buffer: &mut CodeBuffer,
    module: &Module,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(plan) = comparison_shape(module, instr) else {
        return Ok(false);
    };
    let Some((truthy, falsy)) = result_words(module) else {
        return Ok(false);
    };
    let Some(a_source) = cmp_source(plan.a, allocation) else {
        return Ok(false);
    };
    let Some(b_source) = cmp_source(plan.b, allocation) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    let taken: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Load both operands in instruction order (so a deopt sees the unchanged
    // registers), then guard the ones the word compare needs to be exact.
    load_cmp_source(buffer, a_source, FAST_A, highest_touched)?;
    load_cmp_source(buffer, b_source, FAST_B, highest_touched)?;
    if plan.guard != Guard::None {
        buffer.mov_ri(FAST_C, SMALL_MASK)?;
        if matches!(plan.guard, Guard::Lhs | Guard::Both) {
            guard_small(buffer, FAST_A, fallback)?;
        }
        if matches!(plan.guard, Guard::Rhs | Guard::Both) {
            guard_small(buffer, FAST_B, fallback)?;
        }
    }

    // The comparison, then the operator's result.
    buffer.cmp_rr(FAST_A, FAST_B)?;
    buffer.jcc(plan.op.cond(), taken)?;
    materialize(buffer, falsy, plan.dst_x, allocation, highest_touched)?;
    buffer.jmp(fast_done)?;
    buffer.bind(taken)?;
    materialize(buffer, truthy, plan.dst_x, allocation, highest_touched)?;
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
