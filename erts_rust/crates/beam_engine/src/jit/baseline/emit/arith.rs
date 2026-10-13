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

//! The baseline JIT's guarded small-int arithmetic fast path — the tier's one
//! bounded optimisation (the section comment below documents why it is exact).

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Module, Operand};
use beam_term::{MAX_SMALL, MIN_SMALL, TAG_IMMED1_MASK, TAG_IMMED1_SIZE, TAG_IMMED1_SMALL};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::operands::{constant_word, x_offset, x_register};
use super::{emit_step_and_continue, touch, Allocation};

// ---------------------------------------------------------------------------
// The small-int arithmetic fast path
// ---------------------------------------------------------------------------
//
// The tier's **one** bounded optimisation: `gc_bif` of `erlang:'+'/2`,
// `erlang:'-'/2` and `erlang:'*'/2` over small integers, emitted natively behind
// guards that can only ever *reject* (falling back to `jit_step_one`). It cannot
// produce a value the interpreter step would not have produced:
//
// * the two operands must both be small integers — the same `is_small` the
//   `operand`/`Test` seams use (`(word & 0xF) == 0xF`, `../../beam_term/src/immediate.rs`);
// * `+`/`-` are done on the tagged words, which is exact because
//   `make_small(v) == v * 16 + 15`, so `a_word + b_word - 15 == make_small(va+vb)`
//   (`- 15` becomes `+ 15` for the subtraction form);
// * `*` cannot use that trick (the encoding is affine, not multiplicative), so it
//   **untags** both operands with an arithmetic shift right by `SMALL_SHIFT`
//   (exact for every small, negatives included: `word == v * 16 + 15` and
//   `15 < 16`, so the floor of `word / 16` is exactly `v`), multiplies the
//   untagged values, and re-tags with `p * 16 + 15`;
// * any `i64` overflow of a step, and any result outside the small range, jumps to
//   the helper, which calls the real BIF (`BeamError`/bignum/float path) — so a
//   result that is not a small integer is never fabricated here. `+`/`-` range-check
//   the *tagged* word (the encoding maps the value range onto
//   `[MIN_SMALL_WORD, MAX_SMALL_WORD]` bijectively); `*` must range-check the
//   **untagged** product, because `p * 16` silently wraps for `|p| >= 2^59` and can
//   land back inside the valid tagged-word interval (a wrong result).
//
// Everything else (another BIF, another module, an operand that is not a cached
// x-register or a small constant, a destination that is not a cached x-register)
// keeps today's behaviour: one `jit_step_one` call.

/// The three BIFs the fast path lowers — `erlang:'+'/2`, `erlang:'-'/2` and
/// `erlang:'*'/2`, and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SmallArith {
    /// `erlang:'+'/2`.
    Add,
    /// `erlang:'-'/2`.
    Sub,
    /// `erlang:'*'/2`.
    Mul,
}

/// `TAG_IMMED1_MASK` as an `i64` — the low-nibble mask `is_small` tests with.
pub(crate) const SMALL_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `TAG_IMMED1_SMALL` as an `i64` — the immediate-1 small tag, which is also
/// `make_small`'s addend (`((v << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL)`).
pub(crate) const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The emitted guard compares the masked word against [`SMALL_MASK`] (one
/// register holds both, per the objective's shape), so the mask and the tag must
/// be the same nibble. `beam_term` defines both as `0b1111`.
const _: () = assert!(SMALL_MASK == SMALL_TAG);

/// The shift `make_small` applies (`TAG_IMMED1_SIZE`).
pub(crate) const SMALL_SHIFT: u32 = TAG_IMMED1_SIZE;

/// The raw word of the smallest small integer (`make_small(MIN_SMALL)`).
///
/// `MIN_SMALL * 16` is exactly `i64::MIN`, so the (saturating, hence exact) word
/// is `i64::MIN + 15`.
pub(crate) const MIN_SMALL_WORD: i64 = MIN_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The raw word of the largest small integer (`make_small(MAX_SMALL)`) — which
/// saturates to exactly `i64::MAX`.
pub(crate) const MAX_SMALL_WORD: i64 = MAX_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The scratch machine registers the fast path uses for the two operands, the
/// tag/range temporary, the constant, and the result.
///
/// All are caller-saved (System V AMD64), so clobbering them is free, and none
/// overlaps the block's cached x-registers (`r12`–`r15`) or its reserved `rbx`
/// (x-register base) / `rsp` (the `rt` slot). `r11` is also `emit_helper`'s
/// helper-address scratch, but the helper sequence is emitted *after* the fast
/// path has finished with it.
pub(crate) const FAST_A: Reg = Reg::Rax;
/// The second operand's scratch register.
pub(crate) const FAST_B: Reg = Reg::R8;
/// The tag/range temporary.
pub(crate) const FAST_T: Reg = Reg::R10;
/// The tag mask / arithmetic constant.
pub(crate) const FAST_C: Reg = Reg::Rcx;
/// The computed result word.
pub(crate) const FAST_R: Reg = Reg::R11;

/// A `gc_bif` shape the fast path can lower.
pub(crate) struct FastArith<'a> {
    /// Which of the two BIFs is called.
    pub(crate) op: SmallArith,
    /// The first operand (a cacheable `x(i)` or a small-int constant).
    pub(crate) a: &'a Operand,
    /// The second operand.
    pub(crate) b: &'a Operand,
    /// The x-register index the result is written to.
    pub(crate) dst_x: u32,
}

impl FastArith<'_> {
    /// The x-register indices the shape needs cached, in load order (the
    /// operands first, then the destination) — exactly the order
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
}

/// The fast path's shape test for `instr`, a *purely syntactic* check (it never
/// consults the register allocation, so [`super::regalloc::allocate_registers`] and [`super::emit_block`]
/// agree on which `gc_bif`s the fast path targets).
///
/// The shape is the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` of `args.len() == 2`
/// whose every operand is a cacheable x-register or a small-int constant and whose
/// destination is a cacheable x-register.
pub(crate) fn fast_arith<'a>(module: &Module, instr: &'a Instr) -> Option<FastArith<'a>> {
    let Instr::GcBif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    let (module_name, function_name, import_arity) = import_names(module, *bif)?;
    if module_name != "erlang" || import_arity != 2 || *arity != 2 || args.len() != 2 {
        return None;
    }
    let op = match function_name {
        "+" => SmallArith::Add,
        "-" => SmallArith::Sub,
        "*" => SmallArith::Mul,
        _ => return None,
    };
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER || args.iter().any(|operand| !loadable(operand)) {
        return None;
    }
    Some(FastArith {
        op,
        a: &args[0],
        b: &args[1],
        dst_x,
    })
}

/// Whether `operand` is something the fast path can load into a scratch
/// register: a cacheable x-register or a small-int constant.
fn loadable(operand: &Operand) -> bool {
    match x_register(operand) {
        Some(x) => x < MAX_REGISTER,
        None => small_constant(operand).is_some(),
    }
}

/// The raw `Eterm` word of a **small-int** constant operand (`integer`/
/// `character`), or `None` for anything else — including the non-small `atom`/
/// `nil` constants, which the run-time guard would reject anyway (so refusing
/// them at compile time only saves the emission).
fn small_constant(operand: &Operand) -> Option<i64> {
    constant_word(operand).filter(|word| (word & SMALL_MASK) == SMALL_TAG)
}

/// Resolve an imported `{module,function,arity}` to its names, or `None` when
/// the [`ImportRef`] or one of its atoms is missing from `module`.
///
/// This deliberately reproduces `crate::interpreter`'s (private) `import_target`:
/// the fast path must classify a BIF by exactly the strings the interpreter's
/// `GcBif` arm hands to [`ExecutionContext::call_bif`], or it would lower a call
/// the interpreter routes somewhere else.
pub(super) fn import_names<'m>(module: &'m Module, target: ImportRef) -> Option<(&'m str, &'m str, u32)> {
    let import = module.imports.get(target.0 as usize)?;
    let module_name = module.atom_name(import.module)?;
    let function_name = module.atom_name(import.function)?;
    Some((module_name, function_name, import.arity))
}

/// Where the fast path reads one operand from.
#[derive(Clone, Copy)]
enum FastSource {
    /// A cached x-register (already loaded by the prologue).
    Cached(Reg),
    /// A non-cached x-register, read straight from its `machine.x` slot.
    Memory(u32),
    /// A small-int constant, loaded as a raw `Eterm` word.
    Constant(i64),
}

/// The [`FastSource`] for `operand`, or `None` when it is neither an addressable
/// x-register nor a small constant.
///
/// A non-cached x-register is a `machine.x` read, not a deopt: the register
/// allocator's four-register budget decides only *where* the value is read from,
/// never whether the instruction can be lowered.
fn fast_source(operand: &Operand, allocation: &Allocation) -> Option<FastSource> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => match allocation.slot(x) {
            Some(reg) => Some(FastSource::Cached(reg)),
            None => Some(FastSource::Memory(x)),
        },
        Some(_) => None,
        None => small_constant(operand).map(FastSource::Constant),
    }
}

/// Load `source` into scratch register `reg`, recording a `machine.x` read.
fn load_into(
    buffer: &mut CodeBuffer,
    source: FastSource,
    reg: Reg,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match source {
        FastSource::Cached(src) => buffer.mov_rr(reg, src),
        FastSource::Memory(x) => {
            touch(highest_touched, x);
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
        }
        FastSource::Constant(word) => buffer.mov_ri(reg, word),
    }
}

/// Lower a fast-path-eligible `gc_bif` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the
/// result in the destination (its cached machine register, or its `machine.x`
/// slot when it is beyond the cache budget) and letting the block continue with
/// the next instruction. Returns `false` **without having emitted a byte** when
/// the shape matched ([`fast_arith`]) but an operand names a register outside the
/// engine's bound or the destination is not an addressable x-register — the caller
/// then emits exactly today's `jit_step_one` fallback.
///
/// The emitted shape (see the section comment above for why it is exact):
///
/// ```text
///   mov  rax, a          ; the operand: a cached register, `[rbx + x*8]`, or a
///                        ; movabs of a small constant
///   mov  r8,  b
///   mov  rcx, SMALL_MASK
///   mov  r10, rax ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; a is small?
///   mov  r10, r8  ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; b is small?
///   ; --- erlang:'+'/2 and erlang:'-'/2: tagged-word arithmetic ---
///   mov  r11, rax
///   add  r11, r8            ; sub for '-'
///   jo   fallback           ; not both representable as an i64 tagged word
///   mov  rcx, SMALL_TAG  ; the double-tag adjustment make_small re-adds
///   sub  r11, rcx           ; add for '-'
///   jo   fallback
///   mov  rcx, MIN_SMALL_WORD ; cmp r11, rcx ; jl fallback
///   mov  rcx, MAX_SMALL_WORD ; cmp r11, rcx ; jg fallback
///   ; --- erlang:'*'/2: untag, multiply, range-check the product, re-tag ---
///   sar  rax, SMALL_SHIFT   ; va = word >>a 4 (exact for every small)
///   sar  r8,  SMALL_SHIFT   ; vb
///   mov  r11, rax
///   imul r11, r8            ; va * vb, `jo` on signed overflow
///   jo   fallback
///   mov  rcx, MIN_SMALL ; cmp r11, rcx ; jl fallback  ; the UNTAGGED product:
///   mov  rcx, MAX_SMALL ; cmp r11, rcx ; jg fallback  ; `p * 16` wraps for large p
///   shl  r11, SMALL_SHIFT   ; make_small(p) == p * 16 + 15, safe after the check
///   mov  rcx, SMALL_TAG
///   or   r11, rcx
///   mov  <dst>, r11         ; cached destination: its machine register;
///                           ; otherwise `mov [rbx + dst*8], r11`
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spill, call `jit_step_one`, continue
///    at `pc + 1` when the helper advanced by one, else exit to the driver>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The fast path never spills: the destination's cached register is written
/// back by the next helper call or by the block's own exit spill, and a
/// non-cached destination is written to `machine.x` — the exact slot the spill
/// would write — so the spill-across-blocks discipline is unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_gc_bif(
    buffer: &mut CodeBuffer,
    module: &Module,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(fast) = fast_arith(module, instr) else {
        return Ok(false);
    };
    let Some(a_source) = fast_source(fast.a, allocation) else {
        return Ok(false);
    };
    let Some(b_source) = fast_source(fast.b, allocation) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Load both operands into scratch registers.
    load_into(buffer, a_source, FAST_A, highest_touched)?;
    load_into(buffer, b_source, FAST_B, highest_touched)?;

    // Both operands must be small integers: `(word & 0xF) == 0xF`. The mask and
    // the tag are the same nibble (asserted above), so one register serves both.
    buffer.mov_ri(FAST_C, SMALL_MASK)?;
    for operand in [FAST_A, FAST_B] {
        buffer.mov_rr(FAST_T, operand)?;
        buffer.and_rr(FAST_T, FAST_C)?;
        buffer.cmp_rr(FAST_T, FAST_C)?;
        buffer.jcc(Cond::Ne, fallback)?;
    }

    // Compute the result word in `r11`.
    match fast.op {
        // `+`/`-`: tagged-word arithmetic; an `i64` overflow of either step means
        // the result is not representable as a tagged small word, so the helper
        // computes it.
        SmallArith::Add | SmallArith::Sub => {
            buffer.mov_rr(FAST_R, FAST_A)?;
            if fast.op == SmallArith::Add {
                buffer.add_rr(FAST_R, FAST_B)?;
            } else {
                buffer.sub_rr(FAST_R, FAST_B)?;
            }
            buffer.jcc(Cond::O, fallback)?;
            buffer.mov_ri(FAST_C, SMALL_TAG)?;
            if fast.op == SmallArith::Add {
                buffer.sub_rr(FAST_R, FAST_C)?;
            } else {
                buffer.add_rr(FAST_R, FAST_C)?;
            }
            buffer.jcc(Cond::O, fallback)?;

            // Range check: a tagged word is a valid small integer iff it lies in
            // `[MIN_SMALL_WORD, MAX_SMALL_WORD]` (the encoding `v -> v*16 + 15` maps
            // the value range onto that interval bijectively).
            buffer.mov_ri(FAST_C, MIN_SMALL_WORD)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::L, fallback)?;
            buffer.mov_ri(FAST_C, MAX_SMALL_WORD)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::G, fallback)?;
        }
        // `*`: the tagged words cannot be multiplied directly (`make_small` is
        // affine), so untag both with an **arithmetic** shift right — exact for
        // every small, negatives included, because `word == v * 16 + 15` and
        // `15 < 16` makes the floor of `word / 16` exactly `v`.
        SmallArith::Mul => {
            buffer.sar_ri(FAST_A, SMALL_SHIFT)?;
            buffer.sar_ri(FAST_B, SMALL_SHIFT)?;
            buffer.mov_rr(FAST_R, FAST_A)?;
            buffer.imul_rr(FAST_R, FAST_B)?;
            // A signed overflow means the product is not an `i64`, let alone a small.
            buffer.jcc(Cond::O, fallback)?;

            // Range check the **untagged** product. This is *not* interchangeable
            // with the tagged-word check above: `p * 16` silently wraps for
            // `|p| >= 2^59` and can land back inside `[MIN_SMALL_WORD, MAX_SMALL_WORD]`
            // (e.g. `make_small(MAX_SMALL) * 2` tags to the word `-17`, which the
            // tagged check would accept), so only the untagged value is aliasing-free.
            buffer.mov_ri(FAST_C, MIN_SMALL)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::L, fallback)?;
            buffer.mov_ri(FAST_C, MAX_SMALL)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::G, fallback)?;

            // Re-tag: `make_small(p) == p * 16 + 15`, with the shift now provably
            // lossless (`|p| <= 2^59`).
            buffer.shl_ri(FAST_R, SMALL_SHIFT)?;
            buffer.mov_ri(FAST_C, SMALL_TAG)?;
            buffer.or_rr(FAST_R, FAST_C)?;
        }
    }

    // Success: store the result — in the destination's cached register, or
    // straight into its `machine.x` slot when the budget could not cache it —
    // and skip the fallback.
    match allocation.slot(fast.dst_x) {
        Some(dst_reg) => buffer.mov_rr(dst_reg, FAST_R)?,
        None => {
            buffer.mov_mr(Reg::Rbx, x_offset(fast.dst_x), FAST_R)?;
            touch(highest_touched, fast.dst_x);
        }
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
