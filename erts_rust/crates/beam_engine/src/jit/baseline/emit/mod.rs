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
//! small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` fast path ([`try_inline_gc_bif`]) — and the
//! guarded aggregate readers plus the tuple writer ([`aggregate`]: the cons-cell
//! readers `get_list`/`get_hd`/`get_tl`, the tuple readers `get_tuple_element` /
//! `select_tuple_arity`, and the destructive `set_tuple_element`).
//! Two compilation entry points produce the artifact: [`compile_module`] lowers
//! **every** function of a module up front into one shared, eagerly-compiled
//! [`CompiledModule`], while [`compile_function`] lowers **one** function into its own
//! sealed artifact (the [`CompiledFunction`] the lazy layout fills on first entry).
//! Both share the per-function [`emit_function`]. See `super` for the block ABI (in
//! `runtime`) and the driver that runs the emitted code.
//!
//! **Only [`CACHE_REGISTERS`]`.len()` x-registers are held in machine registers**;
//! every other x-register a block names is addressed **directly in `machine.x`**
//! (`[rbx + x*8]`, `rbx` = the array base the prologue loads from
//! [`RUNTIME_XREGS_DISP`]) — the same slot the exit spill writes — so a block that
//! touches more registers than the cache budget still runs natively instead of
//! deopting. The residents are the block's *most-used* registers, chosen by
//! [`allocate_registers`] from the native touch counts of the block's inlinable
//! instructions, so the registers with the longest live ranges are the ones kept in
//! machine registers; the direct `machine.x` accesses are the fill/spill for the
//! rest. That is safe precisely because the cache membership is fixed for the
//! whole block ([`Allocation`]): an x-register is either *always* held in its
//! machine register (and written back at every block exit) or *never* cached, so
//! the two representations can never disagree. The highest x-register index a block
//! touches natively — cached or direct — is recorded per function (in the function's
//! [`Summary`]); [`compile_module`] merges those into the whole-module [`Summary`],
//! and that whole-module bound is the register-file guard the driver applies when
//! entering an **eager** artifact — the one shared mapping whose seal-time links may
//! tail-call across functions — while the **lazy** layout guards on the single
//! function it is about to enter (see [`enter_compiled`](super::enter_compiled)).
//!
//! **Direct block-to-block linking.** A taken branch to a successor basic block
//! that this compilation emitted does not return its `(func, pc)` word to the
//! driver: [`Linker::emit_exit`] tears down the current block's frame (spilling the
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
//! The **local tail calls** are linked the same way. `call_only` is a plain jump
//! (`$DISPATCH_REL(Dest)`), so it lowers exactly like `jump`; `call_last` additionally
//! pops the current frame *before* the transfer, which the tier does through the
//! `jit_tail_call` helper — the interpreter's own `Machine::pop_frame_checked` — with
//! the module-local target already resolved at compile time, i.e. in the interpreter's
//! `resolve`-then-`pop`-then-transfer order ([`Linker::emit_tail_exit`]). A tail-call
//! target that is not a compiled block's entry, and every `call_ext_*` (which enters
//! code outside this module's blocks), keep the documented deopt.
//!
//! The emitter is split by responsibility across this directory: this file owns the
//! compiled-block shape (the register-file constants, the allocation artifact,
//! [`emit_block`], the deopt-and-continue sequence and the two entry points
//! [`compile_module`] and [`compile_function`] via [`emit_function`]);
//! [`regalloc`] the register allocator, [`operands`] the operand/constant readers,
//! [`r#move`] the inline `move`/`swap`, [`test`] the word-level `test` subset,
//! [`select`] the exact-value switch, [`arith`] the guarded small-int fast path,
//! [`link`] the direct block-to-block linker and [`aggregate`] the guarded cons-cell
//! and tuple readers.
#![allow(unsafe_code)]

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Block, ImportRef, Instr, Module};

use crate::interpreter::{self, LabelMap};
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg, DEFAULT_CAPACITY};

use super::code::{CompiledFunction, CompiledModule, Summary};
use super::runtime::{
    encode_target, ExtCallHelper, PositionHelper, RefreshHelper, StepHelper, EXT_CALL,
    EXT_CALL_LAST, EXT_CALL_ONLY, RUNTIME_XREGS_DISP,
};
use super::{jit_ext_call, jit_set_position, jit_step_one};
use super::runtime::{jit_publish_y, jit_refresh_y};

mod aggregate;
mod arith;
mod compare;
mod frame;
mod link;
mod r#move;
mod operands;
mod regalloc;
mod select;
mod test;

use aggregate::{
    try_inline_get_hd, try_inline_get_list, try_inline_get_tl, try_inline_get_tuple_element,
    try_inline_select_tuple_arity, try_inline_set_tuple_element,
};
use arith::try_inline_gc_bif;
use compare::try_inline_comparison;
use frame::{emit_deallocate, emit_deallocate_return, emit_return};
use link::{emit_frame_teardown, Linker};
use operands::x_offset;
use r#move::{try_inline_move, try_inline_swap};
use select::try_inline_select_val;
use test::try_inline_test;

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
pub(crate) use regalloc::allocate_registers;

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// The callee-saved machine registers a block preserves across its prologue and
/// epilogue (the Rust ABI requires them to survive the call).
///
/// Six registers keep `rsp` 16-byte aligned at the helper call site: the entry
/// `rsp ≡ 8 (mod 16)` (`call` pushed the return address), six 8-byte pushes (48
/// bytes, a multiple of 16) leave it at `8 (mod 16)`, and the 24-byte `rt` slot the
/// prologue reserves by `lea` — a further `24 ≡ 8 (mod 16)` bytes — brings it to
/// `0 (mod 16)`. (`rbp` is released from its conventional frame-pointer role to
/// enlarge the cache below; the emitted addressing is uniformly `mod=10` `disp32`,
/// so `rbp` is usable as a memory base with no RIP-relative special case — see
/// [`crate::jit::emitter`].)
pub(crate) const SAVED_REGISTERS: [Reg; 6] =
    [Reg::Rbx, Reg::Rbp, Reg::R12, Reg::R13, Reg::R14, Reg::R15];

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

/// A block's register allocation: the `(x-register index, machine register)` pairs
/// the prologue loads and the epilogue spills.
#[derive(Default)]
pub(crate) struct Allocation {
    /// The slots, in assignment order (`src` before `dst` within a `move`).
    pub(crate) slots: Vec<(u32, Reg)>,
    /// The **highest pc of the block whose instruction accesses a Y slot**, or `None`
    /// when no instruction of the block does.
    ///
    /// The emitter's two Y-window decisions both read it, and both are compile-time
    /// (so a Y-free block pays nothing at run time):
    ///
    /// * the **prologue** publishes the current frame's Y window iff `y_last.is_some()`
    ///   (a block that may read `JitRuntime::y_base`/`y_end`);
    /// * the **post-deopt refresh** ([`Allocation::refresh_y_after`]) is emitted only
    ///   when a still-later instruction accesses a Y slot.
    ///
    /// Only the arithmetic fast path reads the window (see the Y-operand contract in
    /// [`arith`]): an instruction "accesses a Y slot" exactly when [`arith::uses_y`]
    /// says so. A `move`/`get_list`/… with a Y operand is *not* a window access — it
    /// deopts to the interpreter, which reads `machine.stack` itself.
    pub(crate) y_last: Option<u32>,
}

impl Allocation {
    /// The machine register holding x-register `x`, if it is cached.
    pub(crate) fn slot(&self, x: u32) -> Option<Reg> {
        self.slots
            .iter()
            .find(|(n, _)| *n == x)
            .map(|(_, reg)| *reg)
    }

    /// Whether a helper call made at `pc` — whose `Step::Next` continues the block
    /// in-block at `pc + 1` — must be followed by a Y-window refresh.
    ///
    /// True only when a later instruction of the block still accesses a Y slot: the
    /// helper ran one instruction in the interpreter, which may have pushed a frame,
    /// `trim`med one, or **reallocated** `machine.stack`, so the window the block
    /// published in its prologue may be stale. When no later instruction accesses a Y
    /// slot the refresh is not emitted at all — and when the helper leaves the block to
    /// the driver, the next block's prologue publishes instead, so no driver
    /// round-trip ever refreshes.
    pub(crate) fn refresh_y_after(&self, pc: u32) -> bool {
        matches!(self.y_last, Some(last) if last > pc)
    }
}

/// Lower every `Function`'s every `Block` of `module` into **one** native artifact.
///
/// This is the **eager** layout: every function is lowered up front into a single
/// shared [`CodeBuffer`], and the whole-module block map is registered before any
/// code is emitted, so a taken branch to a successor block — including one of
/// **another** function (`fib/1`'s tail call into `fib/3`) — is linked directly at
/// `bind` time (see [`Linker`]). It is what
/// [`JitBackend::compile`](crate::jit::JitBackend::compile) and
/// [`CodeCache::get_or_compile`](crate::jit::cache::CodeCache::get_or_compile)
/// produce, and the layout every whole-module introspection query answers for.
pub(crate) fn compile_module(module: &Module) -> Result<CompiledModule> {
    let labels = interpreter::build_label_map(module);
    let mut buffer = CodeBuffer::with_capacity(code_capacity_estimate(module))?;
    // A flat `[func][pc]` table, not a `HashMap`: the driver's per-block lookup sits on
    // the hot path of every cross-block jump — a `fib`-style loop re-enters the driver
    // once per iteration — so SipHash on a `(u32, u32)` key is a measurable share of the
    // run. Two indexed `Vec` loads replace it.
    let mut entries: Vec<Vec<Option<*const u8>>> = module
        .functions
        .iter()
        .map(|function| vec![None; function.code.len()])
        .collect();
    // The eager twin of the lazy layout's `CompiledFunction::sub_entries`: the native
    // entry of the continuation sub-block starting at `[func][pc]`, filled by
    // [`emit_shared_sub_blocks`] after every IR block (see that function).
    let mut sub_entries: Vec<Vec<Option<*const u8>>> = module
        .functions
        .iter()
        .map(|function| vec![None; function.code.len()])
        .collect();

    // Reserve one entry label per non-empty compiled block **before** emitting any
    // code, so a branch to a successor block — usually a forward reference, e.g. a
    // `jump` to a block emitted later, a `fail` label, or a block of a **later
    // function** — is resolved by the buffer's ordinary `rel32` fixup machinery at
    // `bind` time, before `seal` flips the mapping to read+execute. Each label is
    // bound at its block's entry offset below, exactly where the driver's `entries`
    // slot points.
    let mut block_labels: BTreeMap<(u32, u32), CodeLabel> = BTreeMap::new();
    for (func_index, function) in module.functions.iter().enumerate() {
        let code = &function.code;
        for block in &function.blocks {
            let start = block.start;
            let end = block.end.min(code.len());
            if start >= end {
                continue;
            }
            block_labels.insert((func_index as u32, start as u32), buffer.label());
        }
    }

    let mut summary = Summary::default();
    for func_index in 0..module.functions.len() {
        let func_summary = emit_function(
            &mut buffer,
            module,
            &labels,
            &block_labels,
            func_index as u32,
            &mut entries[func_index],
        )?;
        summary.merge(&func_summary);
    }

    // The eager layout's continuation sub-blocks: emitted **after every** IR block (so
    // the block regions `entries`/`code_map` describe stay contiguous at the front of
    // the shared mapping) and **before** `seal`, so the driver has a native entry at the
    // `pc + 1` a frame-preserving call returns to instead of one interpreter step per
    // remaining instruction of the block. See [`emit_shared_sub_blocks`].
    for func_index in 0..module.functions.len() {
        let function = &module.functions[func_index];
        emit_shared_sub_blocks(
            &mut buffer,
            module,
            &labels,
            &block_labels,
            func_index as u32,
            &function.code,
            &function.blocks,
            &mut sub_entries[func_index],
        )?;
    }

    buffer.seal()?;
    // The artifact records the module's content fingerprint (computed here once, at
    // creation) so the cross-run cache can identify it without re-hashing the IR.
    Ok(CompiledModule::eager(
        buffer,
        entries,
        sub_entries,
        summary,
        labels,
        crate::jit::cache::module_fingerprint(module),
    ))
}

/// Lower **one** function of `module` into its own native artifact.
///
/// This is the **lazy** layout's unit of work: the artifact ([`CompiledFunction`])
/// owns its own sealed [`CodeBuffer`], and its block map — `(func, pc)` keys, all of
/// them this one function — is registered before any code is emitted, so a branch to
/// a successor block of **this** function is still linked directly. A branch to a
/// block of another function is simply absent from that map, which is exactly the
/// documented unlinkable exit: the block returns the target's `(func, pc)` word to
/// the driver, which compiles that function on entry (see
/// [`CompiledModule::native_entry`](super::code::CompiledModule::native_entry)).
///
/// A **continuation address** — the `pc + 1` a frame-preserving call stores in the live
/// frame — is an address the driver can be re-entered at when the callee returns, and it
/// is generally **not** a block start (only an [`Instr::Label`] is). So after the IR
/// blocks are lowered, [`emit_sub_blocks`] lowers one **sub-block** per continuation
/// address that lies strictly inside a block, giving the driver a native entry there
/// instead of a per-instruction interpreter step for the whole block tail.
///
/// Each function gets its **own** mapping ([`CodeBuffer`]), so a lazy artifact never
/// holds code for a function the run has not entered.
pub(crate) fn compile_function(
    module: &Module,
    labels: &LabelMap,
    func: u32,
) -> Result<CompiledFunction> {
    let function = module
        .functions
        .get(func as usize)
        .ok_or_else(|| crate::error::unsupported("jit: no such function"))?;
    let mut buffer = CodeBuffer::with_capacity(
        function_capacity_estimate(&function.code, function.blocks.len())
            + sub_blocks_capacity_estimate(&function.code, &function.blocks)
            + DEFAULT_CAPACITY,
    )?;
    let mut entries: Vec<Option<*const u8>> = vec![None; function.code.len()];
    let mut block_labels: BTreeMap<(u32, u32), CodeLabel> = BTreeMap::new();
    for block in &function.blocks {
        let start = block.start;
        let end = block.end.min(function.code.len());
        if start >= end {
            continue;
        }
        block_labels.insert((func, start as u32), buffer.label());
    }
    let mut summary = emit_function(&mut buffer, module, labels, &block_labels, func, &mut entries)?;
    let mut sub_entries: Vec<Option<*const u8>> = vec![None; function.code.len()];
    emit_sub_blocks(
        &mut buffer,
        module,
        labels,
        &block_labels,
        func,
        &function.code,
        &function.blocks,
        &mut summary,
        &mut sub_entries,
    )?;
    buffer.seal()?;
    Ok(CompiledFunction {
        buffer,
        entries,
        sub_entries,
        summary,
    })
}

/// Whether `instr` is a **frame-preserving call** — one that stores its return address
/// (`Continuation::Address { func, pc: pc + 1, context: current }`) in the live frame.
///
/// These are exactly the calls whose `pc + 1` the driver can be re-entered at once the
/// callee returns: a non-tail local `call`, a non-tail `call_ext`, `call_fun`/`call_fun2`
/// and `apply`. The tail forms (`call_last`, `call_only`, `call_ext_only`,
/// `call_ext_last`, `apply_last`) reuse the caller's *existing* continuation — they store
/// nothing — so they create no new continuation address.
fn stores_continuation(instr: &Instr) -> bool {
    matches!(
        instr,
        Instr::Call { .. }
            | Instr::CallExt { .. }
            | Instr::CallFun { .. }
            | Instr::CallFun2 { .. }
            | Instr::Apply { .. }
    )
}

/// The extra byte budget the continuation **sub-blocks** of `code` need (see
/// [`emit_sub_blocks`]); the [`function_capacity_estimate`] of the IR blocks does not
/// cover them.
///
/// For every continuation address that gets a sub-block covering `[p, b.end)` this
/// budgets a full block ([`BLOCK_BYTES`]) plus one [`INSTRUCTION_BYTES`] per instruction
/// of that range — the same per-instruction bound [`function_capacity_estimate`] uses.
/// Two calls sharing a sub-block range are budgeted twice (a harmless over-estimate):
/// under-estimating would turn a large function into a `SystemLimit` compile error.
fn sub_blocks_capacity_estimate(code: &[Instr], blocks: &[Block]) -> usize {
    let mut total = 0usize;
    for (pc, instr) in code.iter().enumerate() {
        if !stores_continuation(instr) {
            continue;
        }
        let start = pc + 1;
        if start >= code.len() {
            continue;
        }
        if let Some(block) = blocks.iter().find(|b| b.start < start && start < b.end) {
            let end = block.end.min(code.len());
            if start < end {
                total += BLOCK_BYTES + INSTRUCTION_BYTES * (end - start);
            }
        }
    }
    total
}

/// Lower a native **sub-block** for every continuation address a frame-preserving call
/// in `code` stores, filling `sub_entries`.
///
/// A continuation address `p = pc + 1` of a [`stores_continuation`] call is where the
/// driver resumes once the callee returns. The loader cuts a function into blocks only at
/// [`Instr::Label`]s, so `p` usually lies **inside** a block and the driver would
/// otherwise run the whole block tail `[p, b.end)` through one round-trip-plus-interpreter
/// -step per instruction. Emitting `[p, b.end)` as its own standalone block (through the
/// same [`emit_block`] an IR block uses, so every shape rule, guard and deopt is
/// identical) gives the driver a native entry there instead.
///
/// Each such `p` is lowered independently, against the **IR block** that strictly
/// contains it: a second call nested inside an earlier sub-block's range therefore gets
/// its own sub-block too (the code is duplicated — acceptable, and correctness first).
/// An address that is already a compiled block start (the call is the last instruction of
/// its block), and one at or past the end of the function, need no sub-block and are
/// skipped.
///
/// A sub-block is reached only from the driver (nothing jumps to a continuation address),
/// so it reserves no entry label in `block_labels` and adds no entry to the driver's block
/// table: `Summary::block_count` stays the number of IR blocks, and the artifact's
/// `code_map`/`code_address_range` describe IR blocks only. Its register touches are
/// folded into `summary` as a **superset** (a defensive widening is always safe).
#[allow(clippy::too_many_arguments)]
fn emit_sub_blocks(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    block_labels: &BTreeMap<(u32, u32), CodeLabel>,
    func: u32,
    code: &[Instr],
    blocks: &[Block],
    summary: &mut Summary,
    sub_entries: &mut [Option<*const u8>],
) -> Result<()> {
    for (pc, instr) in code.iter().enumerate() {
        if !stores_continuation(instr) {
            continue;
        }
        let start = pc + 1;
        if start >= code.len() {
            continue;
        }
        // An address that is already a compiled block start is served by the IR block's
        // own entry; only a continuation strictly *inside* an IR block needs a sub-block.
        if block_labels.contains_key(&(func, start as u32)) {
            continue;
        }
        let Some(block) = blocks.iter().find(|b| b.start < start && start < b.end) else {
            continue;
        };
        let end = block.end.min(code.len());
        if start >= end {
            continue;
        }
        let allocation = allocate_registers(module, code, start, end);
        for (x, _) in &allocation.slots {
            touch(&mut summary.highest_cached_register, *x);
            if !summary.cached_registers.contains(x) {
                summary.cached_registers.push(*x);
            }
        }
        let offset = buffer.len();
        emit_block(
            buffer,
            module,
            labels,
            block_labels,
            func,
            code,
            start,
            end,
            &allocation,
            &mut summary.highest_native_register,
        )?;
        // SAFETY: the sub-block was fully emitted (no error), so `offset` is the start of
        // a complete native function inside the buffer; `offset + emitted < capacity` was
        // enforced by every emission call.
        let entry = unsafe { buffer.code_ptr().add(offset) };
        // `start < end <= code.len()`, so the slot always exists.
        sub_entries[start] = Some(entry);
    }
    Ok(())
}

/// Lower the eager layout's **continuation sub-blocks** into `buffer`, filling
/// `sub_entries` — the eager twin of [`emit_sub_blocks`].
///
/// The eager layout lowers every function into one shared mapping up front and its driver
/// entry probes the block table only through `entries` (see
/// [`eager_entry`](super::code::eager_entry)); without this pass a continuation address —
/// the `pc + 1` a frame-preserving call stores, usually strictly inside an IR block — would
/// be executed one interpreter-step-per-instruction by the driver. The mandated
/// `jit_bench` harness compiles the eager layout, so this pass gives the eager artifact the
/// same non-block-start native entries the lazy layout already exposes through
/// [`CompiledFunction::sub_entries`](super::code::CompiledFunction).
///
/// It mirrors the lazy [`emit_sub_blocks`] loop *exactly* except for two deliberate
/// differences, both of which keep the eager **whole-module** introspection surface (a
/// frozen contract asserted byte-exactly by the read-only tests — e.g.
/// `block_count`/`cached_registers`/`highest_*`) untouched:
///
/// * **Allocation source** — the **containing IR block's** allocation
///   ([`allocate_registers`] over `[block.start, block.end)`), not the sub-range's own. The
///   sub-block's instructions are a subset of that block's, so the block's cache set is
///   sound for it (every register the sub-block touches natively is already accounted for:
///   either held resident, or read/written directly in `machine.x`), and — because the
///   whole-module [`Summary`] was built from exactly this allocation when the IR block was
///   emitted — reusing it leaves `cached_registers`/`highest_*` unchanged. A sub-range's own
///   usage-weighted allocation is *not* necessarily a subset of the block's, so calling
///   [`allocate_registers`] on `[start, end)` could widen the summary.
/// * **No summary fold** — [`emit_block`]'s register touches during this pass go to a
///   throwaway sink, so the eager whole-module [`Summary`] is provably untouched (it is
///   idempotent anyway: the containing block already touched the same registers).
///
/// Everything else matches the lazy pass: a `start` at or past the end of the function, one
/// that is already a compiled block start (`block_labels`), or one no IR block strictly
/// contains, is skipped; a second call nested inside an earlier sub-block's range gets its
/// own sub-block. Like the lazy version a sub-block reserves no entry label and adds no
/// `entries` slot, so `block_count`/`code_map`/`code_address_range` keep describing IR
/// blocks only.
#[allow(clippy::too_many_arguments)]
fn emit_shared_sub_blocks(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    block_labels: &BTreeMap<(u32, u32), CodeLabel>,
    func: u32,
    code: &[Instr],
    blocks: &[Block],
    sub_entries: &mut [Option<*const u8>],
) -> Result<()> {
    for (pc, instr) in code.iter().enumerate() {
        if !stores_continuation(instr) {
            continue;
        }
        let start = pc + 1;
        if start >= code.len() {
            continue;
        }
        // An address that is already a compiled block start is served by the IR block's
        // own entry; only a continuation strictly *inside* an IR block needs a sub-block.
        if block_labels.contains_key(&(func, start as u32)) {
            continue;
        }
        let Some(block) = blocks.iter().find(|b| b.start < start && start < b.end) else {
            continue;
        };
        let end = block.end.min(code.len());
        if start >= end {
            continue;
        }
        // The containing IR block's allocation (see the doc comment): sound for the
        // sub-range and identical to what the batch IR-block pass merged into the
        // whole-module summary, so that summary is unchanged. `allocate_registers`
        // clamps `end` to `code.len()` itself, so `block.end` needs no pre-clamp here.
        let allocation = allocate_registers(module, code, block.start, block.end);
        let mut discard: Option<u32> = None;
        let offset = buffer.len();
        emit_block(
            buffer,
            module,
            labels,
            block_labels,
            func,
            code,
            start,
            end,
            &allocation,
            &mut discard,
        )?;
        // SAFETY: the sub-block was fully emitted (no error), so `offset` is the start of
        // a complete native function inside the buffer; `offset + emitted < capacity` was
        // enforced by every emission call.
        let entry = unsafe { buffer.code_ptr().add(offset) };
        // `start < end <= code.len()`, so the slot always exists.
        sub_entries[start] = Some(entry);
    }
    Ok(())
}

/// Lower every non-empty block of function `func` into `buffer`, binding the
/// pre-reserved entry label of each and filling `entries[start]` with its native
/// entry point.
///
/// Returns the function's register/block [`Summary`]. `block_labels` is the block map
/// a taken branch is resolved against: the **whole-module** map for [`compile_module`]
/// (so a cross-function branch links directly) or just this function's blocks for
/// [`compile_function`] (so a cross-function branch is the unlinkable exit the driver
/// turns into compile-on-entry). The reserved label of a block always exists, and no
/// label is created without being bound here, so a buffer always seals.
fn emit_function(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    block_labels: &BTreeMap<(u32, u32), CodeLabel>,
    func: u32,
    entries: &mut [Option<*const u8>],
) -> Result<Summary> {
    let function = &module.functions[func as usize];
    let code = &function.code;
    let mut summary = Summary::default();
    for block in &function.blocks {
        let start = block.start;
        let end = block.end.min(code.len());
        if start >= end {
            // An empty block has no instruction to run natively; the interpreter
            // step handles that address exactly as before.
            continue;
        }
        let allocation = allocate_registers(module, code, start, end);
        for (x, _) in &allocation.slots {
            touch(&mut summary.highest_cached_register, *x);
            if !summary.cached_registers.contains(x) {
                summary.cached_registers.push(*x);
            }
        }

        // Bind this block's reserved entry label to the offset it is about to be
        // emitted at (the slot `entries[start]` will point at). The key always
        // exists: it was reserved for exactly these non-empty blocks.
        buffer.bind(block_labels[&(func, start as u32)])?;
        let offset = buffer.len();
        emit_block(
            buffer,
            module,
            labels,
            block_labels,
            func,
            code,
            start,
            end,
            &allocation,
            &mut summary.highest_native_register,
        )?;
        // SAFETY: the block was fully emitted (no error), so `offset` is the
        // start of a complete native function inside the buffer;
        // `offset + emitted < capacity` was enforced by every emission call.
        let entry = unsafe { buffer.code_ptr().add(offset) };
        // `start < end <= code.len()`, so the slot always exists.
        entries[start] = Some(entry);
        summary.block_count += 1;
    }
    summary.cached_registers.sort_unstable();
    summary.cached_registers.dedup();
    Ok(summary)
}
/// A generous byte-count estimate for the code `module` lowers to.
///
/// A block costs a prologue/epilogue pair plus its inlined
/// instructions; the prologue carries the six callee-saved pushes, the `rt`-slot
/// setup, the position-store helper call ([`emit_position_store`] — the same
/// three-argument indirect-call sequence as one deopt, ~40 bytes) and the cached
/// register loads, so [`BLOCK_BYTES`] covers it. A `move`/`swap` whose operands live
/// in `machine.x` is at most two
/// loads plus two stores (~28 bytes), so [`INSTRUCTION_BYTES`] is budgeted per
/// instruction. A `gc_bif` costs either the helper call sequence (~50 bytes) or,
/// when the small-int fast path lowers it, the whole guarded sequence plus its
/// helper fallback ([`FAST_PATH_BYTES`]); a `get_list`/`get_hd`/`get_tl` likewise
/// costs its guarded sequence plus the helper fallback it may still branch to
/// ([`LIST_PATH_BYTES`]), and a `get_tuple_element`/`set_tuple_element`/
/// `select_tuple_arity` its own guarded sequence plus one spill-and-return stub per
/// arity alternative ([`TUPLE_PATH_BYTES`]).
/// Under-estimating would turn a large module into a `SystemLimit` compile error,
/// so the estimate is deliberately loose — [`FAST_PATH_BYTES`] is budgeted for
/// *every* `gc_bif` and [`LIST_PATH_BYTES`] for every cons-cell read, whether or
/// not the fast path claims it, and [`CodeBuffer::with_capacity`] rounds the total
/// up to whole pages.
fn code_capacity_estimate(module: &Module) -> usize {
    module
        .functions
        .iter()
        .map(|function| {
            function_capacity_estimate(&function.code, function.blocks.len())
                + sub_blocks_capacity_estimate(&function.code, &function.blocks)
        })
        .sum::<usize>()
        + DEFAULT_CAPACITY
}

/// The per-function part of [`code_capacity_estimate`]; also the base of the lazy
/// layout's per-function buffer size ([`compile_function`], which adds its own
/// [`DEFAULT_CAPACITY`] headroom).
fn function_capacity_estimate(code: &[Instr], blocks: usize) -> usize {
    let instructions = code.len();
    let mut gc_bifs = 0usize;
    let mut cmp_bifs = 0usize;
    let mut list_reads = 0usize;
    let mut tuple_paths = 0usize;
    for instr in code {
        match instr {
            Instr::Bif { .. } => cmp_bifs += 1,
            Instr::GcBif { .. } => {
                gc_bifs += 1;
                cmp_bifs += 1;
            }
            Instr::GetList { .. } | Instr::GetHd { .. } | Instr::GetTl { .. } => list_reads += 1,
            Instr::GetTupleElement { .. } => tuple_paths += 1,
            Instr::SetTupleElement { .. } => tuple_paths += 1,
            Instr::SelectTupleArity { destinations, .. } => tuple_paths += 1 + destinations.len(),
            _ => {}
        }
    }
    BLOCK_BYTES * blocks
        + INSTRUCTION_BYTES * instructions
        + FAST_PATH_BYTES * gc_bifs
        + COMPARE_PATH_BYTES * cmp_bifs
        + LIST_PATH_BYTES * list_reads
        + TUPLE_PATH_BYTES * tuple_paths
}
/// The code-size budget reserved for one compiled block (see
/// [`code_capacity_estimate`]): the prologue (six callee-saved pushes, the 24-byte
/// `rt`-slot reservation, the `rbx` load, the [`emit_position_store`] helper call)
/// and the epilogue (the frame teardown and `ret`) plus their padding.
const BLOCK_BYTES: usize = 192;

/// The code-size budget reserved for one instruction (see
/// [`code_capacity_estimate`]).
///
/// Every instruction the block does not lower natively is emitted as the full
/// [`emit_step_and_continue`] sequence — the exit spill, the three helper arguments,
/// the indirect call, the continuation check and the reload ([`Fill`]) — which is
/// the largest per-instruction lowering the emitter produces (~150 bytes with five
/// cached registers). Budgeting for the deopt form for *every* instruction also
/// covers the inlined forms, which are smaller.
const INSTRUCTION_BYTES: usize = 192;

/// The code-size budget reserved for one `gc_bif` (see
/// [`code_capacity_estimate`]): the guarded small-int fast path (~140 bytes with
/// `machine.x` operand loads and a `machine.x` result store, ~150 for the
/// `erlang:'*'/2` sequence, which adds the untag/re-tag shifts and a second
/// constant compare) plus the [`emit_step_and_continue`] fallback it branches to
/// (~150 bytes).
const FAST_PATH_BYTES: usize = 384;

/// The code-size budget reserved for one comparison BIF (see
/// [`code_capacity_estimate`]): the two operand loads (~20 bytes), the two small-int
/// guards with their mask materialisation (~40), the compare/branch (~10), both
/// `true`/`false` materialisations with their skip jumps (~45) and the
/// [`emit_step_and_continue`] fallback it branches to (~150, also covered by the
/// per-instruction [`INSTRUCTION_BYTES`]). Deliberately loose: under-estimating
/// would turn a large module into a `SystemLimit` compile error.
const COMPARE_PATH_BYTES: usize = 256;

/// The code-size budget reserved for one `get_list`/`get_hd`/`get_tl` (see
/// [`code_capacity_estimate`]): the guarded cons-cell read — a mask/test/branch, the
/// pointer computation and the CAR/CDR loads with their stores to cached or
/// `machine.x` destinations (~90 bytes) — emitted *in front of* the
/// [`emit_step_and_continue`] fallback it may still branch to, which the
/// per-instruction [`INSTRUCTION_BYTES`] already budgets.
const LIST_PATH_BYTES: usize = 128;

/// The code-size budget reserved for one `get_tuple_element` — and one
/// `select_tuple_arity` *alternative* (see [`code_capacity_estimate`]): the guarded
/// header read (the boxed test, the pointer computation, the arityval test and the
/// range compare, ~70 bytes) plus — for the arity switch — one `mov_ri`/`cmp`/`jcc`
/// and its spill-and-return stub (~45 bytes each). It is emitted *in front of* the
/// [`emit_step_and_continue`] fallback the guard may still branch to, which the
/// per-instruction [`INSTRUCTION_BYTES`] already budgets.
const TUPLE_PATH_BYTES: usize = 160;

/// Emit one native function for `code[start..end]` (a basic block of `func`).
///
/// `highest_touched` accumulates the highest x-register index the emitted code
/// touches natively (a cached load/store, or a direct `machine.x` access); the
/// driver refuses native entry to a machine whose register file does not cover it.
#[allow(clippy::too_many_arguments)]
fn emit_block(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    block_labels: &BTreeMap<(u32, u32), CodeLabel>,
    func: u32,
    code: &[Instr],
    start: usize,
    end: usize,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    // --- prologue -----------------------------------------------------------
    for reg in SAVED_REGISTERS {
        buffer.push_r(reg)?;
    }
    // Reserve the `rt` slot. `lea rsp, [rsp - 24]` is `sub rsp, 24` and keeps `rsp`
    // 16-byte aligned at the helper call site (see `SAVED_REGISTERS`).
    buffer.lea(Reg::Rsp, Reg::Rsp, -24)?;
    buffer.mov_mr(Reg::Rsp, RUNTIME_SLOT_DISP, Reg::Rdi)?;
    // rbx = rt->x_regs, the base of `machine.x` the cached slots are indexed from.
    // Loaded **before** the position store below: `rbx` is callee-saved, so the helper
    // call preserves it, and the cached x-registers are loaded only *after* the call,
    // so nothing a block keeps live across the call sits in caller-saved scratch.
    buffer.mov_rm(Reg::Rbx, Reg::Rdi, RUNTIME_XREGS_DISP)?;
    // Record this block's own `(func, pc)` as the machine's execution position, so
    // `Machine::current_code` (and the `process_info/2` position items built from it)
    // advances at block granularity even across a **linked** branch, which never
    // returns to the driver. A parked `receive` and a raise stay exact because both go
    // through the interpreted deopt (`jit_step_one`), which records the *instruction*
    // address itself. See `jit_set_position`.
    //
    // A block that accesses a Y slot ([`Allocation::y_last`]) publishes the current
    // frame's Y window through `jit_publish_y` **instead**, which does the position
    // store *and* refreshes the window in the one call — so publishing costs such a
    // block nothing extra, and a Y-free block keeps emitting the plain position store
    // (it pays literally nothing). See `jit_publish_y` for why the prologue is the
    // sound, cheapest publish point.
    if allocation.y_last.is_some() {
        emit_publish_y(buffer, func, start as u32)?;
    } else {
        emit_position_store(buffer, func, start as u32)?;
    }
    for (x, reg) in &allocation.slots {
        touch(highest_touched, *x);
        buffer.mov_rm(*reg, Reg::Rbx, x_offset(*x))?;
    }

    let done: CodeLabel = buffer.label();
    // The direct block-to-block linker: a taken branch to a compiled successor's
    // entry leaves this block's frame and jumps straight there (see [`link`]).
    let linker = Linker::new(block_labels);

    // --- body ---------------------------------------------------------------
    //
    // Every instruction of the block is covered: either it is lowered natively, or
    // it is executed by one `jit_step_one` call after which the block **continues**
    // natively at `pc + 1` whenever the helper advanced by exactly one instruction
    // (`Step::Next`) — see [`emit_step_and_continue`]. Only a resolved `Jump`
    // terminates the block early; a helper that produced any other outcome
    // (`Goto`/`Halt`/`Wait`/`Err`) returns to the driver with that word.
    let mut terminated = false;
    for (i, instr) in code[start..end].iter().enumerate() {
        let pc = (start + i) as u32;
        match instr {
            // Metadata / no-ops (`ops.tab:103-108`): they never change control flow.
            // `func_info` is **not** here: it is the failed-clause raise (see the
            // interpreter's `Instr::FuncInfo` arm), so it must deopt through
            // `jit_step_one` rather than be inlined as a no-op (which would fall
            // through to the entry label and re-run the clause tests forever).
            Instr::Label(_)
            | Instr::Line(_)
            | Instr::IntCodeEnd
            | Instr::ExecutableLine { .. }
            | Instr::DebugLine { .. } => {}
            Instr::Move { src, dst } => {
                if !try_inline_move(
                    buffer,
                    src,
                    dst,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // `swap A B` exchanges two x-registers; a block normally caches fewer
            // registers than the instruction touches, so this is a common deopt it
            // is cheap to remove (a three-`mov` shuffle, register or memory).
            Instr::Swap { a, b } => {
                if !try_inline_swap(buffer, a, b, allocation, highest_touched)? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The guarded cons-cell readers: `get_list`/`get_hd`/`get_tl` split a
            // real cons cell natively (see [`aggregate`]). Any other shape — a `Y`
            // operand, a register past `MAX_REGISTER` — is the `_` arm, and so is a
            // **non-cons source**: the guard branches to the helper, whose
            // interpreter step raises the exact `{badmatch, Source}`.
            Instr::GetList { source, head, tail } => {
                if !try_inline_get_list(
                    buffer,
                    source,
                    head,
                    tail,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            Instr::GetHd { source, head } => {
                if !try_inline_get_hd(
                    buffer,
                    source,
                    head,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            Instr::GetTl { source, tail } => {
                if !try_inline_get_tl(
                    buffer,
                    source,
                    tail,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The guarded tuple readers: `get_tuple_element` reads one element of a
            // real tuple natively and `select_tuple_arity` switches on its header
            // word (see [`aggregate`]). Any shape that is not two (resp. one)
            // in-bound x-registers is the `_` arm, and so is a **non-tuple source**:
            // the guard branches to the helper, whose interpreter step raises the
            // exact `{badmatch, Source}` — or reports the typed `unsupported` the
            // arity switch uses for a non-tuple.
            Instr::GetTupleElement {
                source,
                element,
                dst,
            } => {
                if !try_inline_get_tuple_element(
                    buffer,
                    source,
                    *element,
                    dst,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The guarded tuple writer: `set_tuple_element` overwrites one element word
            // of a real tuple natively and destructively (see [`aggregate`]). Any shape
            // that is not an in-bound x-register tuple plus an immediate-or-register
            // value is the `_` arm, and so is a **rejected target**: the guard branches to
            // the helper, whose interpreter step raises the exact `{badmatch, Source}` — a
            // non-tuple or an out-of-range `offset` — or reports the typed `unsupported`
            // for a term in a shared literal area.
            Instr::SetTupleElement {
                element,
                tuple,
                offset,
            } => {
                if !try_inline_set_tuple_element(
                    buffer,
                    element,
                    tuple,
                    *offset,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // Like the `select_val` arm below, this instruction **always** transfers
            // control (the interpreter returns a `Step::Goto` for every path), so the
            // block ends here when the switch is inlined.
            Instr::SelectTupleArity {
                tuple,
                fail,
                destinations,
            } => {
                if try_inline_select_tuple_arity(
                    buffer,
                    labels,
                    &linker,
                    tuple,
                    *fail,
                    destinations,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    terminated = true;
                    break;
                }
                emit_step_and_continue(buffer, done, allocation, func, pc)?;
            }
            // The one optimised BIF family: the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2`.
            // When the guarded fast path is emitted the block keeps going; otherwise
            // (any other BIF, an un-addressable operand, or a register the budget
            // could not cache) it is exactly the `_` arm below.
            Instr::GcBif { .. } => {
                let inlined = try_inline_gc_bif(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? || try_inline_comparison(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )?;
                if !inlined {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The comparison BIFs (`erlang`'s `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`),
            // which the compiler emits as a **non-collecting** `bif` (see [`compare`]).
            // Any other BIF, an un-addressable operand, or a module without both
            // `true`/`false` interned is the `_` arm below.
            Instr::Bif { .. } => {
                if !try_inline_comparison(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // `deallocate N` — the **checked frame pop**. What a compiled block cannot
            // do itself is address the call-frame stack: `Machine::frames`/`stack` are
            // Rust-side `Vec`s with no layout a block could compute an offset into, and
            // the pop is *checked* (a `Deallocate` operand that no longer matches the
            // live frame must raise the interpreter's own error). So the pop goes
            // through the dedicated `jit_deallocate` helper (the [`frame`] submodule), which
            // makes the interpreter's own `Machine::pop_frame_checked` call and answers
            // "the next instruction" on success or `ERROR` (the pop's typed diagnostic)
            // otherwise — the interpreter's arm is `pop_frame_checked` then `Step::Next`.
            // Unlike a `jit_step_one` deopt this pays no interpreter dispatch, no `Step`
            // decode and **no register refill** (the pop touches no x-register), so the
            // block continues in-block on success.
            //
            // The adjacent `deallocate N; return` pair — the loader fuses it into
            // `Instr::DeallocateReturn` (`beam_loader::lower::fuse`), but a hand-built
            // module (the crate's own test modules, the bench fixtures) still holds the
            // separate pair — is lowered as **one** helper round-trip, the same shape the
            // loaded IR pays, with the `return`'s own `pc` recorded (the position the
            // interpreter would have recorded last in that code shape).
            Instr::Deallocate(n) => {
                if matches!(code.get(start + i + 1), Some(Instr::Return)) {
                    emit_deallocate_return(buffer, allocation, done, func, pc + 1, *n)?;
                    terminated = true;
                    break;
                }
                emit_deallocate(buffer, allocation, done, func, pc, *n)?;
            }
            // `return` — follow the current frame's continuation (the frame is **not**
            // popped; that is `deallocate`'s job). The dedicated `jit_return` helper makes
            // the interpreter's own `Machine::continuation()` read and encodes it as a
            // block-ABI word, so the block leaves with it in `rax`: a `(func, pc)` target
            // the driver enters (including a continuation **sub-block**), `HALT` on the
            // bottom frame, or `ENTER` for a caller in another module context. A `return`
            // always transfers control (the interpreter's `return_step` never yields
            // `Step::Next`), so the block ends here.
            Instr::Return => {
                emit_return(buffer, allocation, done, func, pc)?;
                terminated = true;
                break;
            }
            // The loader's fused superinstruction — the shape **every** compiler-produced
            // function body ends with. One `jit_deallocate_return` helper round-trip: the
            // interpreter's checked pop, then the continuation read, in the interpreter's
            // own order.
            Instr::DeallocateReturn(n) => {
                emit_deallocate_return(buffer, allocation, done, func, pc, *n)?;
                terminated = true;
                break;
            }
            Instr::Jump { target } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    // A compiled successor is entered **directly** (the linker tears
                    // this block's frame down and jumps to its entry); anything else
                    // returns the `(func, pc)` word to the driver exactly as before.
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc)?;
                    terminated = true;
                    break;
                }
                // An undefined label keeps the interpreter's exact `unknown_label`
                // error: hand the jump to the helper rather than inventing one.
                Err(_) => emit_step_and_continue(buffer, done, allocation, func, pc)?,
            },
            // A **local tail call** (`call_last Arity Label Deallocate`) transfers
            // control to another block — usually of another function — after popping
            // the current frame. The label is module-local, so it is resolved here, at
            // compile time; when the successor is a compiled block's entry the frame is
            // popped by the interpreter's own checked pop (`jit_tail_call`, in the
            // interpreter's resolve-then-pop order) and control jumps straight to the
            // successor's native entry (see [`link`]). A label the module does not
            // define, or a successor this compilation emitted no block for, keeps the
            // documented deopt: the helper runs the interpreter's own `call_last` and so
            // reports its exact `unknown_label`/frame error. Like `jump`, the
            // instruction always transfers control, so the block ends here either way.
            Instr::CallLast {
                target, deallocate, ..
            } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) if linker.is_compiled(target_func, target_pc) => {
                    linker.emit_tail_exit(
                        buffer,
                        allocation,
                        done,
                        target_func,
                        target_pc,
                        *deallocate,
                    )?;
                    terminated = true;
                    break;
                }
                _ => emit_step_and_continue(buffer, done, allocation, func, pc)?,
            },
            // A local tail call **without** a frame to pop (`call_only Arity Label`):
            // `i_call_only(CallDest)` is `$DISPATCH_REL(Dest)`, so it lowers exactly
            // like the `jump` arm above — resolve at compile time, then link or deopt.
            Instr::CallOnly { target, .. } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc)?;
                    terminated = true;
                    break;
                }
                Err(_) => emit_step_and_continue(buffer, done, allocation, func, pc)?,
            },
            // The **cross-module** calls. Unlike a local `call_*` these enter code
            // outside this module's compiled blocks, so the transfer is always a
            // driver round-trip — but the *call itself* no longer needs an
            // interpreter step: [`emit_ext_call`] performs the import resolution and
            // the variant's frame discipline in the `jit_ext_call` helper, which
            // hands the resolved callee back as the `ENTER` word. Everything the
            // host cannot run in this machine (the synchronous `call_bif` path and
            // every one of its errors) is exactly the interpreter's own step, taken
            // by the helper's fallback.
            Instr::CallExt { target, .. } => {
                emit_ext_call(
                    buffer, allocation, done, func, pc, *target, EXT_CALL, 0,
                )?;
            }
            Instr::CallExtOnly { target, .. } => {
                emit_ext_call(
                    buffer,
                    allocation,
                    done,
                    func,
                    pc,
                    *target,
                    EXT_CALL_ONLY,
                    0,
                )?;
            }
            Instr::CallExtLast {
                target,
                deallocate,
                ..
            } => {
                emit_ext_call(
                    buffer,
                    allocation,
                    done,
                    func,
                    pc,
                    *target,
                    EXT_CALL_LAST,
                    *deallocate,
                )?;
            }
            // The word-level `test` subset (see the section below): a native
            // predicate plus a branch to the resolved fail label. Any other shape —
            // a `NO_FAIL` test, an unresolved fail label, a boxed-term type test, an
            // inexact comparison — is the `_` arm.
            Instr::Test { .. } => {
                if !try_inline_test(
                    buffer,
                    module,
                    labels,
                    &linker,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The exact-value switch subset (see the section below): a chain of
            // word compares against the immediate alternatives, each branching to its
            // resolved target. Any other shape — a `NO_FAIL` switch, a non-immediate
            // case value (a float/bignum/literal), an unresolved label, a `Y`/float/
            // literal scrutinee — is the `_` arm.
            //
            // Unlike the `test` arm this instruction **always** transfers control
            // (the interpreter returns a `Step::Goto` for every path), so the block
            // ends here.
            Instr::SelectVal { .. } => {
                if try_inline_select_val(
                    buffer,
                    labels,
                    &linker,
                    instr,
                    allocation,
                    done,
                    highest_touched,
                )? {
                    terminated = true;
                    break;
                }
                emit_step_and_continue(buffer, done, allocation, func, pc)?;
            }
            // Everything else: one interpreter step, then continue at `pc + 1`.
            _ => emit_step_and_continue(buffer, done, allocation, func, pc)?,
        }
    }

    if !terminated {
        // The block ran off its end: hand the next address back to the driver — or,
        // when the following block starts exactly there, jump straight into it.
        linker.emit_exit(buffer, allocation, done, func, end as u32)?;
    }

    // --- epilogue -----------------------------------------------------------
    //
    // The shared exit every deopt path jumps to (a helper that produced a
    // non-`Next` outcome, a returned `fail`/case target word). Those paths have
    // already spilled the cached x-registers, so the epilogue only leaves the
    // frame — the exact inverse of the prologue.
    buffer.bind(done)?;
    emit_frame_teardown(buffer)?;
    buffer.ret()
}

/// Emit the prologue's position store: call `jit_set_position(rt, func, pc)` so the
/// machine records this block's entry `(func, pc)` as its current execution position.
///
/// `rt` is read out of the prologue-reserved stack slot ([`RUNTIME_SLOT_DISP`]), so
/// this may be emitted anywhere after that slot is written. The helper clobbers only
/// caller-saved registers, so a block must have loaded nothing it needs into them at
/// the call; every cached x-register and `rbx` live in callee-saved registers, which
/// the helper preserves ([`emit_block`] emits this before loading the cached
/// x-registers for exactly that reason). `rsp` is already 16-byte aligned at the call
/// (see [`SAVED_REGISTERS`]). The helper returns nothing — a position store cannot
/// fail — so `rax` is not consulted.
fn emit_position_store(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    let helper: PositionHelper = jit_set_position;
    emit_prologue_helper(buffer, func, pc, helper as *const ())
}

/// Emit the prologue's **Y-window publish**: call `jit_publish_y(rt, func, pc)`, which
/// records this block's entry `(func, pc)` as the machine's position *and* re-publishes
/// the current frame's Y window.
///
/// A block whose body accesses a Y slot ([`Allocation::y_last`]) emits this in place of
/// [`emit_position_store`] — same three-argument call shape, so the block pays no extra
/// call for publishing. The window must be live before the block's first Y access
/// because the Erlang stack is a `Vec` a frame push may reallocate; see `jit_publish_y`.
fn emit_publish_y(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    let helper: PositionHelper = jit_publish_y;
    emit_prologue_helper(buffer, func, pc, helper as *const ())
}

/// Emit the prologue's three-argument helper call — `helper(rt, func, pc)` — shared by
/// [`emit_position_store`] and [`emit_publish_y`] (the two differ only in the helper's
/// body).
///
/// `rt` is read out of the prologue-reserved stack slot ([`RUNTIME_SLOT_DISP`]), so this
/// may be emitted anywhere after that slot is written. The helper clobbers only
/// caller-saved registers, so a block must have loaded nothing it needs into them at
/// the call; every cached x-register and `rbx` live in callee-saved registers, which
/// the helper preserves ([`emit_block`] emits this before loading the cached
/// x-registers for exactly that reason). `rsp` is already 16-byte aligned at the call
/// (see [`SAVED_REGISTERS`]). Neither helper returns a value, so `rax` is not consulted.
fn emit_prologue_helper(buffer: &mut CodeBuffer, func: u32, pc: u32, helper: *const ()) -> Result<()> {
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_ri(Reg::Rsi, i64::from(func))?;
    buffer.mov_ri(Reg::Rdx, i64::from(pc))?;
    buffer.mov_ri(Reg::R11, helper as i64)?;
    buffer.call_r(Reg::R11)
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
/// [`jit_ext_call`]), which must already hold the values the inline prefix produced.
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
fn emit_helper_call(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    helper: *const (),
    args: &[i64],
) -> Result<()> {
    spill(buffer, allocation)?;
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
///   refills the cached x-registers (mandatory: the helper *is* the interpreter and
///   wrote `machine.x`) and **falls through** to the next instruction's lowering;
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
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Emit the native arm for one `call_ext*` instruction: the `jit_ext_call` helper
/// (import resolution + frame discipline) followed by the same continuation check a
/// deopt uses.
///
/// The two outcomes the emitted code distinguishes are exactly the tier's:
///
/// * `encode_target(func, pc + 1)` — the helper fell back to the interpreter, which
///   ran the instruction and advanced by one (the synchronous `call_bif` path), so
///   the cached x-registers are refilled and the block continues at `pc + 1`;
/// * anything else — the `ENTER` word of a resolved cross-module transfer (this arm's
///   whole point), and `HALT`/`ERROR`/`WAIT` — jumps to the block epilogue with the
///   word in `rax`, which the driver decodes exactly as it decodes a deopt's.
///
/// `kind`/`deallocate` select the variant ([`EXT_CALL`], [`EXT_CALL_ONLY`],
/// [`EXT_CALL_LAST`] + the frame size to pop).
#[allow(clippy::too_many_arguments)]
fn emit_ext_call(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    target: ImportRef,
    kind: u32,
    deallocate: u32,
) -> Result<()> {
    let helper: ExtCallHelper = jit_ext_call;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[
            i64::from(func),
            i64::from(pc),
            i64::from(target.0),
            i64::from(kind),
            i64::from(deallocate),
        ],
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Emit the shared tail of a helper sequence: continue in-block when the helper's word
/// in `rax` is exactly "the next instruction" (`Step::Next`), otherwise leave the block
/// with that word in `rax` for the driver to decode.
///
/// The refill is what makes the continuation legal: every helper that can produce
/// `Next` is the interpreter and wrote `machine.x`, so the block's cached x-registers
/// are stale.
fn emit_continue_check(
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
    fill(buffer, allocation)?;
    // Continuing in-block past the helper: the instruction it ran may have pushed or
    // popped a frame (moving the Y window) or reallocated `machine.stack` (freeing the
    // published one), so re-publish the window before the next instruction when a later
    // instruction of this block still accesses a Y slot. The gate is compile-time, so a
    // block that never touches Y emits nothing here, and the refresh sits on the
    // *continue* arm only — a helper that leaves the block to the driver pays nothing
    // (the next block's prologue publishes).
    if allocation.refresh_y_after(pc) {
        emit_refresh_y(buffer)?;
    }
    Ok(())
}

/// Write every cached x-register back into `machine.x` (`rbx` holds the base).
pub(super) fn spill(buffer: &mut CodeBuffer, allocation: &Allocation) -> Result<()> {
    for (x, reg) in &allocation.slots {
        buffer.mov_mr(Reg::Rbx, x_offset(*x), *reg)?;
    }
    Ok(())
}

/// Load every cached x-register out of `machine.x` (`rbx` holds the base).
///
/// The mandatory counterpart of [`spill`] after a helper call: the helper ran the
/// interpreter on `machine.x`, so the block's cached values are stale — every one of
/// them may have been changed (an instruction can write any x-register). Reloading
/// **all** of them is the liveness model's conservative form: no per-register
/// liveness is needed, and a register that was not actually touched is reloaded with
/// the value it already holds.
fn fill(buffer: &mut CodeBuffer, allocation: &Allocation) -> Result<()> {
    for (x, reg) in &allocation.slots {
        buffer.mov_rm(*reg, Reg::Rbx, x_offset(*x))?;
    }
    Ok(())
}
