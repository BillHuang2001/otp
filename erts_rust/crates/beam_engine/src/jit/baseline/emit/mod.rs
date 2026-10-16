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

//! The baseline JIT's emitter: typed IR → x86-64 basic blocks.
//!
//! The register allocator ([`allocate_registers`]) and the per-instruction
//! lowering ([`emit_block`]) — the inline `move`/`swap`/`jump`/metadata set, the
//! word-level `test` and exact-value `select_val` subsets, the
//! deopt-and-continue sequence ([`emit_step_and_continue`]) that covers every
//! instruction the block does not lower natively, the spill/fill discipline
//! ([`CACHE_REGISTERS`]), the tier's one arithmetic optimisation — the guarded
//! small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` fast path ([`arith::try_inline_gc_bif`]) — and the
//! guarded aggregate readers plus the tuple writer ([`aggregate`]: the cons-cell
//! readers `get_list`/`get_hd`/`get_tl`, the tuple readers `get_tuple_element` /
//! `select_tuple_arity`, and the destructive `set_tuple_element`).
//! Two compilation entry points produce the artifact — in [`compile`]: [`compile_module`]
//! lowers **every** function of a module up front into one shared, eagerly-compiled
//! [`CompiledModule`](super::code::CompiledModule), while [`compile_function`] lowers
//! **one** function into its own sealed artifact (the
//! [`CompiledFunction`](super::code::CompiledFunction) the lazy layout fills on first
//! entry). Both share the per-function [`emit_function`](compile::emit_function). See
//! `super` for the block ABI (in `runtime`) and the driver that runs the emitted code.
//!
//! **Only [`CACHE_REGISTERS`]`.len()` x-registers are held in machine registers**;
//! every other x-register a block names is addressed **directly in `machine.x`**
//! (`[rbx + x*8]`, `rbx` = the array base the prologue loads from
//! [`RUNTIME_XREGS_DISP`](super::runtime::RUNTIME_XREGS_DISP)) — the same slot the exit
//! spill writes — so a block that
//! touches more registers than the cache budget still runs natively instead of
//! deopting. The residents are the registers with the **longest live intervals**,
//! chosen by [`allocate_registers`] from the native touches of the block's inlinable
//! instructions, so the registers that stay live across the most instruction
//! boundaries are the ones kept in machine registers; the direct `machine.x`
//! accesses are the fill/spill for the rest. That is safe precisely because the cache
//! membership is fixed for the whole block ([`Allocation`]): an x-register is either
//! *always* held in its
//! machine register (and written back at every block exit) or *never* cached, so
//! the two representations can never disagree. The highest x-register index a block
//! touches natively — cached or direct — is recorded per function (in the function's
//! [`Summary`](super::code::Summary)); [`compile_module`] merges those into the
//! whole-module [`Summary`](super::code::Summary), and that whole-module bound is the
//! register-file guard the driver applies when entering an **eager** artifact — the one
//! shared mapping whose seal-time links may tail-call across functions — while the
//! **lazy** layout guards on the single function it is about to enter (see
//! [`enter_compiled`](super::enter_compiled)).
//!
//! **Direct block-to-block linking.** A taken branch to a successor basic block
//! that this compilation emitted does not return its `(func, pc)` word to the
//! driver: [`link::Linker::emit_exit`] tears down the current block's frame (spilling the
//! cached x-registers first — the tier's whole liveness/root-set model) and jumps
//! **straight to the successor block's native entry**, whose own prologue rebuilds
//! its frame. Both entry points reserve one entry label per block *before* emitting
//! any code, so a forward branch is an ordinary `rel32` fixup resolved at `bind`
//! time — never a post-`seal` patch, so the W^X policy is unchanged. A branch is
//! linked only when its target is in the **same compilation's** block map:
//! [`compile_module`]'s map spans the whole module (so a cross-function branch links
//! directly), while [`compile_function`]'s covers just that function, so a branch to
//! another function returns its word and the driver compiles that function on entry.
//! Every
//! continuation that is **not** a compiled block's entry (an address inside a
//! block, an empty block, an unresolved label, a function's end) keeps the
//! documented deopt: spill, materialise the `encode_target` word in `rax`, and jump
//! to the block epilogue for the driver to decode.
//!
//! The **local tail calls** are lowered natively too. `call_only` is a plain jump
//! (`$DISPATCH_REL(Dest)`), so it lowers exactly like `jump`; `call_last` additionally
//! pops the current frame *before* the transfer, which the tier does through the
//! `jit_tail_call` helper — the interpreter's own `Machine::pop_frame_checked` — with
//! the module-local target already resolved at compile time, i.e. in the interpreter's
//! `resolve`-then-`pop`-then-transfer order. A successor that is a compiled block's
//! entry is linked directly ([`link::Linker::emit_tail_exit`]); a successor that is a
//! **resolved** label this compilation emitted no block for — the common case for a
//! tail call into another function of the lazy per-function artifact — takes the same
//! checked pop and then leaves the resolved `(func, pc)` word in `rax` for the driver
//! ([`link::Linker::emit_tail_exit_to_driver`]), which is exactly the interpreter's
//! `Step::Goto`. Only an **undefined** label deopts, and every `call_ext_*` (which
//! enters code outside this module's blocks) keeps its own documented deopt.
//!
//! The emitter is split by responsibility across this directory: this file owns the
//! compiled-block shape (the register-file constants, the allocation artifact, the
//! [`Allocation`] register cache with its spill/fill traffic, and the deopt-and-continue
//! sequence [`emit_step_and_continue`]/[`emit_continue_check`] every arm of the dispatch
//! shares); [`block`] the per-instruction lowering dispatch ([`emit_block`] itself, the
//! prologue position store / Y publish, the deopt wrapper and the helper-sequence arms);
//! [`compile`] the compilation entry points ([`compile_module`]/[`compile_function`] via
//! [`emit_function`](compile::emit_function)) and the continuation sub-blocks;
//! [`regalloc`] the register allocator, [`liveness`] the block-local fill-set
//! computation, [`operands`] the operand/constant readers,
//! [`r#move`] the inline `move`/`swap`, [`test`] the word-level `test` subset,
//! [`select`] the exact-value switch, [`arith`] the guarded small-int fast path,
//! [`link`] the direct block-to-block linker, [`compare`] the comparison BIFs,
//! [`bif`] the `erlang:hd/1`/`element/2`/`tuple_size/1` accessors, and [`aggregate`]
//! the guarded cons-cell and tuple readers.
#![allow(unsafe_code)]

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::runtime::{encode_target, RefreshHelper, StepHelper};
use super::runtime::jit_refresh_y;
use super::jit_step_one;

mod aggregate;
pub(super) mod arith;
mod bif;
mod block;
mod compare;
mod compile;
mod family;
mod frame;
mod heap;
mod link;
mod liveness;
mod r#move;
mod operands;
mod prologue;
mod regalloc;
mod regs;
mod select;
mod test;

// The dirty-set store model's unit tests (`Allocation::spill_sets` /
// `spill_slots_at` / `spill`), plus the consistency pin over the committed
// fixture corpus. A separate file because the module's own unit tests live in
// `baseline/tests.rs`, which is not a child of this node.
#[cfg(test)]
#[path = "spill_tests.rs"]
mod spill_tests;
// The register cache — the `Allocation` and its spill/fill traffic — lives in the
// `regs` submodule; re-exported here so `emit::Allocation`, `emit::spill`,
// `emit::fill` and the item paths the other emitter submodules name stay unchanged.
pub(crate) use regs::{Allocation, FillSite, SpillSite};
use regs::{fill, spill};

// The names this module's own unit tests (`baseline/tests.rs` glob-imports `emit`)
// reach as `emit::NAME`, re-exported here so that splitting the emitter into
// submodules changed no item path. Only the tests use them, hence `cfg(test)`.
#[cfg(test)]
pub(crate) use arith::{
    fast_arith, uses_y, FastDest, SmallArith, FAST_A, FAST_B, FAST_C, FAST_R, FAST_T,
    MAX_SMALL_WORD, MIN_SMALL_WORD, SMALL_MASK, SMALL_SHIFT, SMALL_TAG,
};
#[cfg(test)]
pub(crate) use operands::{constant_word, x_register, y_addressable, y_register};
#[cfg(test)]
pub(crate) use r#move::{move_shape, MoveShape};
#[cfg(test)]
pub(crate) use regalloc::cacheable_touches;
#[cfg(test)]
pub(crate) use regalloc::is_inlined;
pub(crate) use regalloc::allocate_registers;

// The compilation entry points live in the `compile` submodule; re-export them here
// so every item path outside this module (`emit::compile_module`,
// `emit::compile_function` — named by `baseline/mod.rs` and `baseline/code.rs`) stays
// exactly what it was.
pub(crate) use compile::{compile_function, compile_module};

// The per-instruction lowering dispatch lives in the `block` submodule; re-export its
// entry point here so every item path outside this module (`emit::emit_block` — named by
// `compile.rs`, which emits each block's code, and by the other emitter submodules' docs)
// stays exactly what it was.
pub(crate) use block::emit_block;

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// The **canonical** full callee-saved register set the block ABI may touch (the Rust
/// ABI requires them to survive the call), in prologue push order.
///
/// A block does **not** save all six: it saves only the subset [`native_frame_regs`]
/// returns — `rbx` (always, the `machine.x` base) plus the machine registers it actually
/// holds as cache slots. The other registers are used **only** as cache registers and as
/// caller-saved scratch (`r9`/`r8`/`r11`/`r10`/`rax`/`rcx`), so a block with no slot for
/// one never writes it and needs no save. `rsp` stays 16-byte aligned at the helper call
/// site because the prologue reserves [`frame_slot_len`]`(k)` bytes for the `rt` slot,
/// which compensates the `k` pushes a block actually makes.
/// (`rbp` is released from its conventional frame-pointer role to enlarge the cache; the
/// emitted addressing is uniformly `mod=10` `disp32`, so `rbp` is usable as a memory base
/// with no RIP-relative special case — see [`crate::jit::emitter`].)
pub(crate) const SAVED_REGISTERS: [Reg; 6] =
    [Reg::Rbx, Reg::Rbp, Reg::R12, Reg::R13, Reg::R14, Reg::R15];

/// The machine registers a block actually saves in its prologue and restores in its
/// teardown.
///
/// `rbx` (the `machine.x` base, always loaded in the prologue) followed by exactly the
/// machine registers the block holds as cache slots ([`Allocation::slots`], in slot order).
/// The single source of truth for **both** directions of the frame, so the prologue and
/// the teardown can never drift apart.
pub(super) fn native_frame_regs(allocation: &Allocation) -> Vec<Reg> {
    let mut regs = Vec::with_capacity(1 + allocation.slots.len());
    regs.push(Reg::Rbx);
    regs.extend(allocation.slots.iter().map(|(_, reg)| *reg));
    // Every register saved must be one of the canonical callee-saved set the block ABI may
    // touch: saving a caller-saved scratch register would be pointless, and (worse) would
    // clamp a value the helper needs. This pins the invariant the prologue/teardown rely on.
    debug_assert!(
        regs.iter().all(|reg| SAVED_REGISTERS.contains(reg)),
        "a saved frame register must be callee-saved"
    );
    regs
}

/// The number of bytes the prologue reserves for the `rt` slot, chosen so `rsp` is
/// 16-byte aligned at every helper call site given that the block pushes
/// `k = native_frame_regs(allocation).len()` 8-byte registers.
///
/// A block entry always has `rsp ≡ 8 (mod 16)` (the driver's `call` — or a linked
/// predecessor's `jmp` — left the original return address on the stack) and the pushes
/// leave it at `8 − 8k (mod 16)`; the reservation `slot` must then satisfy
/// `8 − 8k − slot ≡ 0 (mod 16)` with `slot ≥ 8`, i.e. `slot = 8` when `k` is even and `16`
/// when `k` is odd.
pub(super) fn frame_slot_len(k: usize) -> i32 {
    if k % 2 == 0 {
        8
    } else {
        16
    }
}

/// The machine registers a block caches x-registers in — `rbx` is reserved for the
/// machine base pointer, so five x-registers fit (`rbp` included).
///
/// The assignment order is historical: the four established slots keep their
/// `r12`–`r15` registers and the released `rbp` is appended as the fifth, so a
/// register that was cacheable before the pool was widened stays in the same
/// physical register and the slot↔register mapping is stable across the change.
pub(crate) const CACHE_REGISTERS: [Reg; 5] =
    [Reg::R12, Reg::R13, Reg::R14, Reg::R15, Reg::Rbp];

/// The byte offset of the prologue-reserved `rt` slot from `rsp`.
pub(crate) const RUNTIME_SLOT_DISP: i32 = 0;

/// The caller-saved scratch registers a block uses to shuffle values between
/// `machine.x` slots (a `move`/`swap` whose source, destination, or both are not
/// cached), and for the raw `Eterm` word of a constant stored into `machine.x`.
///
/// Both are caller-saved (System V AMD64), so clobbering them across an instruction
/// boundary is free; neither is a cache register ([`CACHE_REGISTERS`]) nor the
/// block's reserved `rbx` (the `machine.x` base) / `rsp` (the `rt` slot). They are
/// used strictly *within* one instruction's lowering — no value survives into the
/// next instruction — so they may also be reused by the small-int fast path's
/// temporaries.
pub(crate) const MOVE_SCRATCH: Reg = Reg::R11;
/// The second `machine.x`-shuffle scratch register (`swap` needs two values in
/// flight at once).
pub(crate) const MOVE_SCRATCH_2: Reg = Reg::R10;

/// Record that x-register `x` is touched natively by the block (loaded into a
/// cache register, or addressed directly in `machine.x`).
pub(super) fn touch(highest: &mut Option<u32>, x: u32) {
    *highest = Some(match *highest {
        Some(previous) => previous.max(x),
        None => x,
    });
}

/// Emit the **gated Y-window refresh**: call `jit_refresh_y(rt)` so the block's next Y
/// access compares against the live frame.
///
/// Emitted on the *continue* arm of an in-block helper call ([`emit_continue_check`])
/// only when a later instruction of the block still accesses a Y slot
/// ([`Allocation::refresh_y_after`]): the helper ran one instruction in the interpreter,
/// which may have pushed a frame, `trim`med one, or reallocated `machine.stack`. The
/// register arguments are the same as a deopt's (`rt` from the block's own stack slot);
/// the helper clobbers only caller-saved registers and returns nothing.
fn emit_refresh_y(buffer: &mut CodeBuffer) -> Result<()> {
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    let helper: RefreshHelper = jit_refresh_y;
    buffer.mov_ri(Reg::R11, helper as *const () as i64)?;
    buffer.call_r(Reg::R11)
}

/// Emit the spill-then-helper sequence for `helper` — **without** the block exit, so
/// the caller decides what to do with the helper's word.
///
/// The spill comes **first**: every helper reads `machine.x` (the interpreter itself
/// for [`jit_step_one`], the frame discipline of `call_ext_last` for
/// [`jit_ext_call`](super::jit_ext_call)), which must already hold the values the inline prefix produced.
/// The `rt` slot is reloaded into `rdi` (every helper's first argument) and `args`
/// fills the remaining integer argument registers in ABI order (`rsi`, `rdx`, `rcx`,
/// `r8`, `r9`); a helper preserves the callee-saved registers, so `rbx` (the
/// `machine.x` base) survives the call — but the block must **not** spill after it:
/// the helper legitimately wrote `machine.x`.
///
/// The helper's result word is left in `rax` (the block ABI's return register) for
/// the caller's continuation check. Passing the helper as a raw pointer keeps the
/// call sites' own function-pointer types (`StepHelper`, `ExtCallHelper`) as the
/// type check on its signature.
pub(super) fn emit_helper_call(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    helper: *const (),
    args: &[i64],
pc: u32) -> Result<()> {
    spill(buffer, allocation, SpillSite::Helper, pc)?;
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    // The `rsi`/`rdx`/`rcx`/`r8`/`r9` argument registers, in ABI order.
    const ARG_REGISTERS: [Reg; 5] = [Reg::Rsi, Reg::Rdx, Reg::Rcx, Reg::R8, Reg::R9];
    for (reg, value) in ARG_REGISTERS.iter().zip(args) {
        buffer.mov_ri(*reg, *value)?;
    }
    buffer.mov_ri(Reg::R11, helper as i64)?;
    buffer.call_r(Reg::R11)
}

/// Emit "execute instruction `(func, pc)` with one interpreter step, then **continue
/// natively** when it advanced by exactly one instruction".
///
/// This is the tier's per-instruction deopt. `jit_step_one` maps its outcome onto
/// the block ABI:
///
/// * `Step::Next` → `encode_target(func, pc + 1)` — the helper executed exactly the
///   instruction at `pc` and the block can carry on at `pc + 1`, so the emitted code
///   refills the cached x-registers the continuation can still observe (mandatory: the
///   helper *is* the interpreter and wrote `machine.x`; see [`emit_continue_check`] for
///   why the refill is partial) and **falls through** to the next instruction's
///   lowering;
/// * any other word (`Goto`/`HALT`/`ERROR`/`ENTER`, and the `Wait` → `blocked`
///   error) → jump to the block epilogue with that word in `rax`, exactly as before.
///
/// Continuing in-block rather than leaving the block is what makes native coverage
/// independent of where a block's first non-inlinable instruction sits: the
/// surrounding instructions still run natively, and the driver's per-instruction
/// entry lookup is paid once per block instead of once per instruction.
pub(super) fn emit_step_and_continue(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    allocation: &Allocation,
    func: u32,
    pc: u32,
) -> Result<()> {
    let helper: StepHelper = jit_step_one;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    pc)?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Emit the shared tail of a helper sequence: continue in-block when the helper's word
/// in `rax` is exactly "the next instruction" (`Step::Next`), otherwise leave the block
/// with that word in `rax` for the driver to decode.
///
/// The refill is what makes the continuation legal: every helper that can produce
/// `Next` is the interpreter and wrote `machine.x`, so a cached register whose
/// `machine.x` slot it wrote is stale. It is a **partial** refill
/// ([`regs::fill`]): only the cached registers [`Allocation::fill_slots`] lists — the
/// continuation's block-local liveness unioned with the block's write set, see
/// [`liveness`] — are reloaded. A cached register left out keeps a stale word on
/// purpose, which is safe because it is never read (a later read through its machine
/// register makes it live here) and never stored (every store writes exactly the block's
/// write set, which the fill set always contains).
pub(super) fn emit_continue_check(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    allocation: &Allocation,
    func: u32,
    pc: u32,
) -> Result<()> {
    // `rax` holds the helper's word; only "the next instruction" continues here.
    buffer.mov_ri(Reg::R11, encode_target(func, pc + 1))?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)?;
    fill(buffer, allocation, FillSite::Helper, pc)?;
    // Continuing in-block past the helper: the instruction it ran may have pushed or
    // popped a frame (moving the Y window) or reallocated `machine.stack` (freeing the
    // published one), so re-publish the window before the next instruction when a later
    // instruction of this block still accesses a Y slot. The gate is compile-time, so a
    // block that never touches Y emits nothing here, and the refresh sits on the
    // *continue* arm only — a helper that leaves the block to the driver pays nothing
    // (the next block's prologue publishes).
    if allocation.refresh_y_after(pc) {
        #[cfg(test)]
        super::counters::record_y_invalidation();
        emit_refresh_y(buffer)?;
    }
    Ok(())
}
