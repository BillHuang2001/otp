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

//! The string-compare shape: `bs_match_string`.
//!
//! `bs_match_string Fail Ctx Bits Offset` compares the next `Bits` bits of the match
//! context against the module's `StrT` string at byte `Offset`
//! (`i_bs_match_string`, `erts/emulator/beam/emu/bs_instrs.tab:1004`). It is how the
//! compiler spells a **binary literal inside a match** — `<<"GET ", Rest/binary>>`,
//! `<<"HTTP/1.", _/binary>>`, `<<"ok\n">>` — i.e. the protocol-prefix compare every parser
//! runs once per message. Nothing is bound, so the whole effect is one branch and one
//! word store.
//!
//! # The pattern is a compile-time constant, copied into the code
//!
//! `Module::strings` holds the `StrT` chunk's raw bytes, so `Offset` and the pattern are
//! both known when the block is compiled — and the pattern's bytes are therefore emitted as
//! **`movabs` immediates**, one per compared byte. Neither the offset nor a **pointer** is
//! ever baked into the code: a cached [`crate::jit::cache::CompiledModule`] may be run
//! against a `Module` whose string and literal areas are *different allocations*
//! (`super::super::r#move`'s literal guard exists for exactly that reason — it re-reads the
//! area's base from `JitRuntime` on every access instead of baking it). An immediate has no
//! such lifetime.
//!
//! # The fast path is a straight-line byte compare
//!
//! The subject's readable window is the one [`emit_subject_view`] classifies, and the bit
//! position of the first compared bit inside it is the context's `start` (an on-heap
//! `ErlHeapBits`' own bit offset is always `0`, so `off` contributes nothing). The emitted
//! sequence therefore guards `start % 8 == 0` at run time and then compares **whole
//! bytes**: `Bits / 8` full bytes, plus — for a `Bits` that is not a whole number of bytes
//! — one final byte whose **high** `Bits % 8` bits are compared by masking both sides with
//! the same immediate. Both halves are exercised by `bitstr_inline_tests.rs`: the
//! byte-multiple rows and the masked-tail row (a 12-bit pattern, the `<<"ab", 4-bit>>`
//! shape) assert the value against the interpreter, and a mismatch at the **last** bit of a
//! masked tail is a row of its own.
//!
//! A `start` that is **not** byte-aligned takes the `fallback` — one interpreter step, so
//! the family's own bit-by-bit `bits_equal` answers and the row is merely slower. That is
//! the shape a preceding sub-byte `bs_skip_bits2`/`bs_get_integer2` produces, and it is
//! pinned as the arm's legitimate-deopt row.
//!
//! # The size cap
//!
//! The sequence is straight-line code, so an unbounded pattern would be an unbounded
//! instruction: [`MAX_MATCH_BYTES`] caps the compared bytes at **64** (a 512-bit pattern) and
//! a longer one is refused at compile time, emitting **zero bytes** — the family's own loop
//! is the right home for it. 64 bytes covers every protocol literal a parser compares per
//! message, and it keeps the per-instruction code-size budget
//! ([`super::super::BS_MATCH_STRING_PATH_BYTES`]) at a couple of kilobytes.
//!
//! # The mutation order is the whole correctness argument
//!
//! A `fallback` is a **deopt**: `jit_step_one` re-executes the *whole* instruction in the
//! interpreter with the same operand, so any state a partial execution committed is visible
//! to that re-execution. The one mutation here is the context's `start` word, so the emitted
//! order is: **every** guard (the match-context guard, the remaining-window check, the
//! alignment guard, the subject classification, the readable-bounds guard), then the byte
//! compares (which mutate nothing), then the **single** `start` write. The fail arm writes
//! nothing at all. This is the rule the `bs_match` arm's per-command commit broke once (see
//! `../bs_match_deopt_tests.rs`); here it is trivially satisfiable and
//! `bitstr_inline_tests.rs`'s non-aligned row is the pin that a fallback after the compares
//! still answers what the interpreter answers.
//!
//! # No helper, and no position store
//!
//! The whole shape is arithmetic and comparison, so no helper is called — which is why the
//! arm emits **no position store**: the `machine.current_code` publication
//! (`super::super::prologue::emit_position_store`) is what a helper call or an instruction
//! body re-entered from the driver needs, and the only exits here are the in-block
//! `fast_done` continuation, the linker's fail-label exit (`Linker::emit_exit`, which
//! carries the `pc` it stores) and the `fallback`'s own `emit_step_and_continue`, which
//! stores the position itself. `super::tail`'s two shapes leave the block on exactly the
//! same three edges and likewise emit none.

use beam_common::Result;
use beam_loader::{Label, Module, Operand, NO_FAIL};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::Loc;
use super::{
    emit_ctx_guard, emit_step_and_continue, emit_subject_view, Allocation, MOVE_SCRATCH,
    MOVE_SCRATCH_2, SUB_BITS_END_DISP, SUB_BITS_START_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_BS_MATCH_STRING};

// ---------------------------------------------------------------------------
// Shape 10: `bs_match_string { fail, ctx, bits, offset }`
// ---------------------------------------------------------------------------

/// The largest pattern this shape compares in-block, in bytes.
///
/// The emitted sequence is straight-line code — one `movabs`–`cmp`–`jcc` triple per compared
/// byte — so this cap *is* the shape's code-size ceiling: 64 bytes of pattern cost at most
/// `64 * ~26 + <guards>` bytes of generated code, which is what
/// [`super::super::BS_MATCH_STRING_PATH_BYTES`] budgets for. A pattern longer than this is
/// refused at compile time (zero bytes emitted) and stays on the family's own loop, which is
/// the right home for a very wide literal.
pub(crate) const MAX_MATCH_BYTES: usize = 64;

/// The operands of a `bs_match_string` the tier lowers natively.
#[derive(Clone, Debug)]
pub(crate) struct MatchStringShape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The number of bits the pattern must match, a compile-time constant.
    bits: u32,
    /// The `ceil(bits / 8)` pattern bytes the compare reads, **copied out of the module's
    /// string table** at compile time (`Offset..Offset + ceil(bits / 8)`). Never a pointer
    /// and never a bare offset: see the module docs.
    pattern: Vec<u8>,
}

impl MatchStringShape {
    /// The [`MatchStringShape`] the four operands name, or `None` when the whole instruction
    /// must stay on the family.
    ///
    /// Purely syntactic, **label-free** and **offset-free** — the fail-label resolution is
    /// the caller's, and so is everything the pattern's *length* beyond the compare's own
    /// bytes could affect — so the block's Y gate ([`super::super::arith::uses_y`]) derives
    /// the same answer as the emitter without needing a `LabelMap`.
    ///
    /// The compile-time conditions, each a refusal rather than an approximation:
    ///
    /// * `ctx` is an in-bound x-register or an addressable `Y` slot ([`Loc::of`]);
    /// * `Offset` names a byte **inside** the module's string table. `get(offset..)` is
    ///   `Some(&[])` exactly at `offset == len`, so a single `Option` tells the legitimately
    ///   empty tail from an out-of-range offset — the latter is the family's own typed
    ///   `unsupported("bs_match_string: string table offset out of range")`, which a shape
    ///   predicate must not pre-empt;
    /// * the pattern the compare reads is at most [`MAX_MATCH_BYTES`] bytes (the code-size
    ///   cap above);
    /// * `Bits` does not exceed the string table's own remaining **bit** count. When it
    ///   does, the family can only ever take its fail branch (`size > pattern.len() * 8`),
    ///   so declining keeps that degenerate row on the family instead of emitting a
    ///   permanently-failing sequence.
    ///
    /// Every other property — a `Bits` that is not a whole number of bytes, a `Bits` of zero —
    /// is handled exactly by the emitted sequence (see the module docs), not declined.
    pub(crate) fn of(
        module: &Module,
        ctx: &Operand,
        bits: u32,
        offset: u32,
    ) -> Option<MatchStringShape> {
        let ctx = Loc::of(ctx)?;
        let pattern = match_string_pattern(module, bits, offset)?;
        Some(MatchStringShape { ctx, bits, pattern })
    }

    /// The x-register indices the shape touches — the context only.
    pub(crate) fn registers(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// The x-register indices the shape **writes**: none. It binds nothing; the commit stores
    /// into the context block's `start` word, which is a heap store, not an x-register write.
    pub(crate) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The x-register indices the shape **reads** — the context.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.registers()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y()
    }
}

/// The `ceil(bits / 8)` pattern bytes `Offset` and `Bits` name, or `None` when the shape is
/// one this tier does not lower (see [`MatchStringShape::of`] for the list).
fn match_string_pattern(module: &Module, bits: u32, offset: u32) -> Option<Vec<u8>> {
    let table = module.strings.get(offset as usize..)?;
    let bits = bits as usize;
    let whole = bits / 8;
    let tail = bits % 8;
    let nbytes = whole + usize::from(tail != 0);
    if nbytes > MAX_MATCH_BYTES {
        return None;
    }
    // `size > pattern.len() * 8` is the family's own always-fails branch; refusing here keeps
    // that row on the family rather than emitting a sequence that can never succeed.
    if bits > table.len().saturating_mul(8) {
        return None;
    }
    // `nbytes <= table.len()` follows from `bits <= table.len() * 8`, so the slice cannot
    // panic; `get` keeps that a checked fact rather than an argument.
    table.get(..nbytes).map(<[u8]>::to_vec)
}

/// The inlinable shape of a `bs_match_string`, or `None` when the whole instruction must
/// stay on the family.
///
/// A `NO_FAIL` fail code *raises* `{badmatch, Subject}` when the pattern does not match, and
/// building that reason allocates — so only a real label is inlined. The label must also
/// resolve in the module's map, because the fail transfer needs its `(function, code)`
/// target; an unresolved label keeps the family's exact `unknown_label`.
pub(crate) fn match_string_shape(
    labels: &LabelMap,
    module: &Module,
    fail: Label,
    ctx: &Operand,
    bits: u32,
    offset: u32,
) -> Option<(MatchStringShape, (u32, u32))> {
    let shape = MatchStringShape::of(module, ctx, bits, offset)?;
    if fail == NO_FAIL {
        return None;
    }
    let target = labels.resolve(fail).ok()?;
    Some((shape, target))
}

/// Emit `bs_match_string Fail Ctx Bits Offset` natively, or report that the block must
/// helper it (see [`super::try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence, in this order — every guard first, then the compares, then the one
/// commit:
///
/// ```text
///   <the match-context guard>            ; not a context -> fallback; r11 = the block base
///   mov  rsi, [ctx + 16]                 ; start
///   mov  rax, [ctx + 24]                 ; end
///   rcx = end.saturating_sub(start)
///   cmp  rcx, Bits ; jb fail_arm         ; fewer remaining bits than the pattern needs
///   test rsi, 7 ; jne fallback           ; a non-byte-aligned start (no pattern bytes -> skipped)
///   <emit_subject_view>                  ; anything but an on-heap ErlHeapBits -> fallback
///                                        ;   r10 = byte base, rax = readable bytes, rsi kept
///   rdi = start / 8
///   lea  rcx, [rdi + ceil(Bits / 8)] ; cmp rcx, num_bytes ; ja fallback
///   add  rdi, r10                        ; the first compared byte's address
///   for k in 0 .. Bits / 8:
///     movzx r8, byte [rdi + k] ; mov r9, Pattern[k] ; cmp r8, r9 ; jne fail_arm
///   (Bits % 8 != 0:)
///     movzx r8, byte [rdi + Bits/8] ; mov r9, Pattern[last]
///     xor   r8, r9 ; mov r9, tail_mask ; test r8, r9 ; jne fail_arm
///   mov  rcx, Bits ; add rsi, rcx
///   mov  [ctx + 16], rsi                 ; THE ONLY MUTATION, after every guard
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>     ; a terminator on that path only
/// fallback:
///   <one interpreter step>                    ; the family's own answer, on an untouched context
/// fast_done:
/// ```
///
/// The readable-bounds guard is emitted **before** the first load, exactly as
/// [`super::emit_bit_extract`] emits its own: the compared bytes must lie inside the
/// subject's allocated data region, so the guard is `first + ceil(Bits / 8) <= num_bytes`
/// against `emit_subject_view`'s **word-padded** byte count. A pattern that would need a
/// byte past that bound takes `fallback` — never an out-of-range read, never a fabricated
/// match.
///
/// `Bits == 0` needs no compare at all: the remaining-window test cannot fail (the family's
/// `saturating_sub(start) < 0` is false for every `start`, including one past `end`), and
/// the commit stores `start + 0`, which is exactly the family's `set_ctx_start` on the
/// vacuously-true `bits_equal(.., 0)` path. The subject is still classified, because the
/// family's `subject_bytes` runs before its `bits_equal` and can raise.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_match_string(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    module: &Module,
    fail: Label,
    ctx: &Operand,
    bits: u32,
    offset: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = match_string_shape(labels, module, fail, ctx, bits, offset) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_BS_MATCH_STRING);

    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    let sat: CodeLabel = buffer.label();

    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;

    // `remaining = end.saturating_sub(start)`, then `remaining < Bits` -> the FAIL label (the
    // family's `fail_step`), never a raise and never a fallback. `Bits == 0` can never take
    // it, which is the family's own `saturating_sub` semantics.
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::Rax, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::Rax)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    buffer.mov_ri(MOVE_SCRATCH_2, i64::from(shape.bits))?;
    buffer.cmp_rr(Reg::Rcx, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::B, fail_arm)?;

    // Classify the inner subject and derive the readable window — one shared helper, so this
    // shape cannot disagree with `bs_get_integer2`/`bs_match` about what a subject is. A
    // literal-area subject, a refc window, a match context, a bare `BinRef` and every
    // non-bitstring take the fallback: the family's own answer, and never a fabricated view.
    let (base, _bit_pos, num_bytes) = emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;

    if !shape.pattern.is_empty() {
        // A byte-aligned `start` is what makes the byte compare below exact: an on-heap
        // subject's own bit offset is 0, so the first compared bit is bit `start % 8` of
        // `base[start / 8]`. Anything else is the family's bit-by-bit `bits_equal`, one
        // interpreter step — never a shifted reimplementation.
        buffer.mov_ri(Reg::Rcx, 7)?;
        buffer.test_rr(Reg::Rsi, Reg::Rcx)?;
        buffer.jcc(Cond::Ne, fallback)?;

        // The over-read guard, emitted BEFORE the first load: `start / 8 + nbytes <=
        // num_bytes`, with `num_bytes` the subject's own word-padded data extent.
        let nbytes = shape.pattern.len();
        buffer.mov_rr(Reg::Rdi, Reg::Rsi)?;
        buffer.shr_ri(Reg::Rdi, 3)?;
        buffer.lea(Reg::Rcx, Reg::Rdi, nbytes as i32)?;
        buffer.cmp_rr(Reg::Rcx, num_bytes)?;
        buffer.jcc(Cond::A, fallback)?;
        buffer.add_rr(Reg::Rdi, base)?;

        let whole = shape.bits as usize / 8;
        let tail = shape.bits as usize % 8;
        for (k, byte) in shape.pattern.iter().take(whole).enumerate() {
            buffer.movzx_byte(Reg::R8, Reg::Rdi, k as i32)?;
            buffer.mov_ri(Reg::R9, i64::from(*byte))?;
            buffer.cmp_rr(Reg::R8, Reg::R9)?;
            buffer.jcc(Cond::Ne, fail_arm)?;
        }
        if tail != 0 {
            // The final partial byte: only its HIGH `tail` bits belong to the pattern, so
            // both sides are masked with the same immediate — `((base ^ pattern) & mask) == 0`
            // is exactly `bits_equal`'s last `tail` comparisons.
            buffer.movzx_byte(Reg::R8, Reg::Rdi, whole as i32)?;
            buffer.mov_ri(Reg::R9, i64::from(shape.pattern[whole]))?;
            buffer.xor_rr(Reg::R8, Reg::R9)?;
            let mask = ((0xFFu16 << (8 - tail)) & 0xFF) as i64;
            buffer.mov_ri(Reg::R9, mask)?;
            buffer.test_rr(Reg::R8, Reg::R9)?;
            buffer.jcc(Cond::Ne, fail_arm)?;
        }
    }

    // The commit: one word store, after every guard and after the compares (which mutate
    // nothing), so a `fallback` anywhere above re-executes the instruction against an
    // untouched context.
    buffer.mov_ri(Reg::Rcx, i64::from(shape.bits))?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;
    buffer.jmp(fast_done)?;

    buffer.bind(fail_arm)?;
    linker.emit_exit(
        buffer,
        allocation,
        done,
        target.0,
        target.1,
        pc,
        ExitSpill::Required,
    )?;

    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
