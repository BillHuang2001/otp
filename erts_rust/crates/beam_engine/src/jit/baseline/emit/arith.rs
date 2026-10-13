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

use super::operands::{constant_word, x_offset, x_register, y_addressable, y_offset, y_register};
use super::{emit_step_and_continue, touch, Allocation, RUNTIME_SLOT_DISP};
use super::super::runtime::{RUNTIME_YBASE_DISP, RUNTIME_YEND_DISP};
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
// Everything else (another BIF, another module, an operand that is a static literal,
// a destination that is a literal or a float register, a register index outside the
// engine's bound) keeps today's behaviour: one `jit_step_one` call.
//
// ## Y-slot operands and a Y-slot destination
//
// A `Y` operand is a slot in the **current call frame's** Y area, which lives in
// `machine.stack` — a Rust `Vec` that a frame push may reallocate. The runtime
// therefore publishes the live frame's Y window (`JitRuntime::y_base`/`y_end`,
// refreshed by the driver before every native entry and by every frame-changing
// helper) and the emitted Y access is **guarded** against it:
//
// ```text
//   mov  reg,       [rsp + RUNTIME_SLOT_DISP]  ; rt
//   mov  scratch,   [reg + RUNTIME_YEND_DISP]  ; y_end
//   mov  reg,       [reg + RUNTIME_YBASE_DISP] ; y_base
//   lea  scratch2,  [reg + k*8]                ; the slot address
//   cmp  scratch2,  scratch
//   jae  fallback                              ; outside the frame -> interpreter
//   mov  reg,       [scratch2]                 ; (the store writes r11 instead)
// ```
//
// The guard can only ever **reject**: a slot outside the frame jumps to the same
// `jit_step_one` fallback as any other un-lowered shape, and the interpreter's own
// `y(k)` bounds check then produces exactly its error — so no out-of-frame
// dereference and no wrong value is possible. The frame-less case is covered too:
// `y_base == y_end == null` makes `lea k*8; cmp; jae` reject every slot. A Y index so
// large that `y_offset` would overflow the `disp32` is refused at compile time
// ([`super::operands::y_addressable`]).
//
// A Y slot is **not** an x-register, so neither a Y operand nor a Y destination is
// charged to `highest_touched` — that bound is the x-register file's
// (`machine.x.len() > bound`), and raising it for a stack slot would wrongly refuse
// native entry.

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

/// Where the fast path writes its result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FastDest {
    /// A cacheable (or directly addressable) x-register.
    X(u32),
    /// A slot in the current call frame's Y area, written through the guarded Y
    /// window (see the section comment).
    Y(u32),
}

/// A `gc_bif` shape the fast path can lower.
pub(crate) struct FastArith<'a> {
    /// Which of the two BIFs is called.
    pub(crate) op: SmallArith,
    /// The first operand (an addressable `x(i)`/`y(k)` or a small-int constant).
    pub(crate) a: &'a Operand,
    /// The second operand.
    pub(crate) b: &'a Operand,
    /// Where the result is written.
    pub(crate) dst: FastDest,
}

impl FastArith<'_> {
    /// The x-register indices the shape needs cached, in load order (the
    /// x-register operands first, then the destination **when it is an
    /// x-register**) — exactly the order [`super::regalloc::allocate_registers`]
    /// caches them in.
    ///
    /// A `Y` operand and a `Y` destination contribute nothing: they are stack slots,
    /// not x-registers, so they must not enter the x-register usage counts (a
    /// spurious entry would evict a real x-register from the cache).
    pub(super) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(3);
        for operand in [self.a, self.b] {
            if let Some(x) = x_register(operand) {
                registers.push(x);
            }
        }
        if let FastDest::X(x) = self.dst {
            registers.push(x);
        }
        registers
    }
}

/// The fast path's shape test for `instr`, a *purely syntactic* check (it never
/// consults the register allocation, so [`super::regalloc::allocate_registers`] and [`super::emit_block`]
/// agree on which `gc_bif`s the fast path targets).
///
/// The shape is the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` of `args.len() == 2`
/// whose every operand is an in-bound x-register, an addressable Y slot or a small-int
/// constant and whose destination is an in-bound x-register or an addressable Y slot.
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
    let dst = destination(dst)?;
    if args.iter().any(|operand| !loadable(operand)) {
        return None;
    }
    Some(FastArith {
        op,
        a: &args[0],
        b: &args[1],
        dst,
    })
}

/// The [`FastDest`] for `operand`, or `None` when it is neither an in-bound
/// x-register nor an addressable Y slot.
fn destination(operand: &Operand) -> Option<FastDest> {
    if let Some(x) = x_register(operand) {
        return (x < MAX_REGISTER).then_some(FastDest::X(x));
    }
    if let Some(k) = y_register(operand) {
        return y_addressable(k).then_some(FastDest::Y(k));
    }
    None
}

/// Whether `operand` is something the fast path can load into a scratch
/// register: an in-bound x-register, an addressable Y slot, or a small-int
/// constant.
fn loadable(operand: &Operand) -> bool {
    if let Some(x) = x_register(operand) {
        return x < MAX_REGISTER;
    }
    if let Some(k) = y_register(operand) {
        return y_addressable(k);
    }
    small_constant(operand).is_some()
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
    /// A Y slot, read through the guarded frame window.
    YSlot(u32),
    /// A small-int constant, loaded as a raw `Eterm` word.
    Constant(i64),
}

/// The [`FastSource`] for `operand`, or `None` when it is neither an addressable
/// x-register, an addressable Y slot nor a small constant.
///
/// A non-cached x-register is a `machine.x` read, not a deopt: the register
/// allocator's four-register budget decides only *where* the value is read from,
/// never whether the instruction can be lowered. A Y slot is likewise read natively,
/// behind the run-time frame-window guard.
fn fast_source(operand: &Operand, allocation: &Allocation) -> Option<FastSource> {
    if let Some(x) = x_register(operand) {
        if x >= MAX_REGISTER {
            return None;
        }
        return Some(match allocation.slot(x) {
            Some(reg) => FastSource::Cached(reg),
            None => FastSource::Memory(x),
        });
    }
    if let Some(k) = y_register(operand) {
        return y_addressable(k).then_some(FastSource::YSlot(k));
    }
    small_constant(operand).map(FastSource::Constant)
}

/// Load `source` into scratch register `reg`, recording a `machine.x` read and
/// jumping to `fallback` when a **Y slot** lies outside the current frame.
///
/// The Y branch is the only one that can transfer control at run time: every other
/// source is a compile-time-addressable x-register slot or an immediate.
fn load_into(
    buffer: &mut CodeBuffer,
    source: FastSource,
    reg: Reg,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match source {
        FastSource::Cached(src) => buffer.mov_rr(reg, src),
        FastSource::Memory(x) => {
            touch(highest_touched, x);
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
        }
        FastSource::YSlot(k) => emit_y_load(buffer, reg, k, fallback),
        FastSource::Constant(word) => buffer.mov_ri(reg, word),
    }
}

/// Emit a **guarded** read of Y slot `k` into `reg`, jumping to `fallback` when the
/// slot lies outside the current frame's Y window.
///
/// ```text
///   mov  reg,      [rsp + RUNTIME_SLOT_DISP]   ; rt (the block's own stack slot)
///   mov  FAST_C,   [reg + RUNTIME_YEND_DISP]   ; y_end
///   mov  reg,      [reg + RUNTIME_YBASE_DISP]  ; y_base
///   lea  FAST_R,   [reg + k*8]                 ; the slot address
///   cmp  FAST_R,   FAST_C
///   jae  fallback                              ; slot >= y_end -> outside the frame
///   mov  reg,      [FAST_R]
/// ```
///
/// Registers are safe: `reg` is [`FAST_A`] or [`FAST_B`] (the operand scratch the
/// caller is filling), and the two temporaries [`FAST_C`]/[`FAST_R`] are
/// caller-saved and dead at operand-load time — the mask constant is loaded into
/// [`FAST_C`] only **after** both operands are loaded, and the result register
/// [`FAST_R`] is written only much later. Reloading `rt` out of the block's own stack
/// slot (`[rsp + RUNTIME_SLOT_DISP]`) is the established idiom
/// ([`super::emit_helper_call`] does the same for every helper call).
///
/// The guard is a pure rejection: the fallback is the caller's `jit_step_one` path,
/// which runs the instruction in the interpreter and reports exactly its own
/// value/error. `y_base == y_end == null` (no live frame) rejects every slot, so the
/// emitted `lea` result is never dereferenced out of frame.
fn emit_y_load(
    buffer: &mut CodeBuffer,
    reg: Reg,
    k: u32,
    fallback: CodeLabel,
) -> Result<()> {
    buffer.mov_rm(reg, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(FAST_C, reg, RUNTIME_YEND_DISP)?;
    buffer.mov_rm(reg, reg, RUNTIME_YBASE_DISP)?;
    buffer.lea(FAST_R, reg, y_offset(k))?;
    buffer.cmp_rr(FAST_R, FAST_C)?;
    buffer.jcc(Cond::Ae, fallback)?;
    buffer.mov_rm(reg, FAST_R, 0)
}

/// Emit the **guarded** store of the fast path's result (`FAST_R`) into Y slot `k`,
/// jumping to `fallback` when the slot lies outside the current frame's Y window.
///
/// ```text
///   mov  FAST_C, [rsp + RUNTIME_SLOT_DISP]   ; rt
///   mov  FAST_A, [FAST_C + RUNTIME_YBASE_DISP] ; y_base
///   mov  FAST_C, [FAST_C + RUNTIME_YEND_DISP]  ; y_end
///   lea  FAST_A, [FAST_A + k*8]              ; the slot address
///   cmp  FAST_A, FAST_C
///   jae  fallback                            ; outside the frame
///   mov  [FAST_A], FAST_R
/// ```
///
/// Emitted only after the arithmetic is complete, so [`FAST_A`] (an operand scratch)
/// and [`FAST_C`] are dead; the value being stored lives in [`FAST_R`]. A rejection
/// falls back to `jit_step_one`, which re-runs the **whole** instruction in the
/// interpreter — nothing has been committed to the machine before this store, so the
/// interpreter sees exactly the state it would have seen.
fn emit_y_store(buffer: &mut CodeBuffer, k: u32, fallback: CodeLabel) -> Result<()> {
    buffer.mov_rm(FAST_C, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(FAST_A, FAST_C, RUNTIME_YBASE_DISP)?;
    buffer.mov_rm(FAST_C, FAST_C, RUNTIME_YEND_DISP)?;
    buffer.lea(FAST_A, FAST_A, y_offset(k))?;
    buffer.cmp_rr(FAST_A, FAST_C)?;
    buffer.jcc(Cond::Ae, fallback)?;
    buffer.mov_mr(FAST_A, 0, FAST_R)
}

/// Lower a fast-path-eligible `gc_bif` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the
/// result in the destination (its cached machine register, its `machine.x` slot
/// when it is beyond the cache budget, or its Y slot behind the frame-window
/// guard) and letting the block continue with the next instruction. Returns
/// `false` **without having emitted a byte** when the shape matched
/// ([`fast_arith`]) but an operand or the destination names neither an
/// in-bound x-register nor an addressable Y slot — the caller then emits exactly
/// today's `jit_step_one` fallback.
///
/// A Y operand or a Y destination additionally carries a **run-time** rejection:
/// the emitted frame-window guard can branch to the same `fallback` when the slot
/// lies outside the current frame, so an in-frame Y `gc_bif` runs natively and a
/// malformed frame keeps the interpreter's exact error.
///
/// The emitted shape (see the section comment above for why it is exact):
///
/// ```text
///   mov  rax, a          ; the operand: a cached register, `[rbx + x*8]`, a
///                        ; movabs of a small constant, or the guarded Y read below
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
///   mov  <dst>, r11         ; cached destination: its machine register; a
///                           ; non-cached one: `mov [rbx + dst*8], r11`; a Y slot:
///                           ; the guarded Y store (see `emit_y_store`)
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

    // Load both operands into scratch registers. A Y operand carries its own
    // frame-window guard, whose rejection jumps to `fallback` before anything is
    // committed.
    load_into(buffer, a_source, FAST_A, fallback, highest_touched)?;
    load_into(buffer, b_source, FAST_B, fallback, highest_touched)?;
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

    // Success: store the result — in the destination's cached register, straight
    // into its `machine.x` slot when the budget could not cache it, or into the
    // destination Y slot behind the same frame-window guard — and skip the fallback.
    match fast.dst {
        FastDest::X(dst_x) => match allocation.slot(dst_x) {
            Some(dst_reg) => buffer.mov_rr(dst_reg, FAST_R)?,
            None => {
                buffer.mov_mr(Reg::Rbx, x_offset(dst_x), FAST_R)?;
                touch(highest_touched, dst_x);
            }
        },
        // A Y destination is not an x-register, so it never raises the x-register
        // bound the driver's entry guard reads.
        FastDest::Y(k) => emit_y_store(buffer, k, fallback)?,
    }
    buffer.jmp(fast_done)?;
    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
