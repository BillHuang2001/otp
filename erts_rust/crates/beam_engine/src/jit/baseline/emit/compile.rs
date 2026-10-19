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

//! Compiling a `beam_loader::Module` (or one function of it) into a native
//! [`CodeBuffer`] artifact.
//!
//! This file owns the compilation entry points and the machinery they share:
//! [`compile_module`] lowers **every** function of a module into one shared,
//! eagerly-compiled [`CompiledModule`], while [`compile_function`] lowers **one**
//! function into its own sealed [`CompiledFunction`] (the lazy layout's unit of
//! work). Both share the per-function [`emit_function`], the native
//! **continuation sub-blocks** that give the driver an entry at the `pc + 1` a
//! frame-preserving call returns to ([`stores_continuation`], [`emit_sub_blocks`]
//! and [`emit_shared_sub_blocks`]), and the code-size estimates
//! ([`code_capacity_estimate`], [`function_capacity_estimate`] and
//! [`sub_blocks_capacity_estimate`]) that size the [`CodeBuffer`]. The
//! per-instruction lowering ([`emit_block`]) and the compiled-block shape live
//! one level up in `super`.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Block, Instr, Module};

use crate::interpreter::{self, LabelMap};
use crate::jit::emitter::{CodeBuffer, Label as CodeLabel, DEFAULT_CAPACITY};

use super::super::code::{CompiledFunction, CompiledModule, Summary};
use super::{allocate_registers, emit_block, touch};

/// Lower every `Function`'s every `Block` of `module` into **one** native artifact.
///
/// This is the **eager** layout: every function is lowered up front into a single
/// shared [`CodeBuffer`], and the whole-module block map is registered before any
/// code is emitted, so a taken branch to a successor block — including one of
/// **another** function (`fib/1`'s tail call into `fib/3`) — is linked directly at
/// `bind` time (see [`Linker`](super::link::Linker)). It is what
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
/// [`CompiledModule::native_entry`](super::super::code::CompiledModule::native_entry)).
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

/// The exclusive end of the sub-block that begins at continuation address `start`:
/// the containing IR block's end, **capped at the next continuation address**.
///
/// `block` is the IR block that strictly contains `start` (`block.start < start <
/// block.end`, `start < code.len()`). The cap is the smallest continuation address
/// `p = pc + 1 > start` of a later [`stores_continuation`] call in the **same
/// function**: the instruction at `p - 1` is that next frame-preserving call, which
/// always transfers control out of the block (a deopt / `ENTER`, or a `call_bif`
/// continue the shared `done` epilogue turns into an `encode_target(func, p)` word),
/// and `p` is itself a continuation address that gets its own sub-block entry. Lowering
/// only `[start, p)` therefore keeps every instruction reachable at a native entry
/// while making the emitted ranges **disjoint** — the total lowered sub-block length is
/// O(code.len()), not the O(n²) of overlapping tails for `n` calls in one straight-line
/// block. When no later call exists the cap is inert and the range runs to the block's
/// end.
fn sub_block_end(code: &[Instr], block: &Block, start: usize) -> usize {
    let block_end = block.end.min(code.len());
    (start..code.len())
        .find(|&pc| stores_continuation(&code[pc]))
        .map_or(block_end, |pc| block_end.min(pc + 1))
}

/// The extra byte budget the continuation **sub-blocks** of `code` need (see
/// [`emit_sub_blocks`]); the [`function_capacity_estimate`] of the IR blocks does not
/// cover them.
///
/// For every continuation address that gets a sub-block covering `[p, end)` — where
/// `end` is [`sub_block_end`], so the ranges are the **disjoint** ones the emitter
/// lowers — this budgets a full block ([`BLOCK_BYTES`]) plus one [`INSTRUCTION_BYTES`]
/// per instruction of that range, the same per-instruction bound
/// [`function_capacity_estimate`] uses. Because the ranges are disjoint and all lie
/// within `[0, code.len())`, their total instruction count is at most `code.len()`, so
/// the estimate stays linear in the function size. It is deliberately loose on the
/// per-range constants: under-estimating would turn a large function into a
/// `SystemLimit` compile error.
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
            let end = sub_block_end(code, block, start);
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
/// Each such `p` is lowered against the **IR block** that strictly contains it, and
/// its range is capped at the **next continuation address** ([`sub_block_end`]), so the
/// emitted sub-block ranges are **disjoint** and the total lowered length stays O(n).
/// The last instruction of a range is the next frame-preserving call, which always
/// transfers control out of the block, and that call's own continuation address has its
/// own sub-block entry — so no instruction loses its native entry (the driver resumes
/// natively at the next sub-block). An address that is already a compiled block start
/// (the call is the last instruction of its block), and one at or past the end of the
/// function, need no sub-block and are skipped.
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
        let end = sub_block_end(code, block, start);
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
/// [`eager_entry`](super::super::code::eager_entry)); without this pass a continuation address —
/// the `pc + 1` a frame-preserving call stores, usually strictly inside an IR block — would
/// be executed one interpreter-step-per-instruction by the driver. The mandated
/// `jit_bench` harness compiles the eager layout, so this pass gives the eager artifact the
/// same non-block-start native entries the lazy layout already exposes through
/// [`CompiledFunction::sub_entries`](super::super::code::CompiledFunction).
///
/// It mirrors the lazy [`emit_sub_blocks`] loop *exactly* except for one deliberate
/// difference, which keeps the eager **whole-module** introspection surface (a frozen
/// contract asserted byte-exactly by the read-only tests — e.g.
/// `block_count`/`cached_registers`/`highest_*`) untouched:
///
/// * **No summary fold** — the allocation is derived for the **emitted range**
///   (`[start, end)`, exactly as the lazy pass derives it), but [`emit_block`]'s register
///   touches during this pass go to a throwaway sink instead of `summary`, so the eager
///   whole-module [`Summary`] is provably untouched: it describes the IR blocks, whose
///   own [`emit_function`] pass already folded exactly those allocations in.
///
/// The allocation must be the sub-range's own, never the containing IR block's: an
/// [`Allocation`] is **entry-point-relative** (see [`super::regs`]) — `block_start`, the
/// per-`pc` `fill_sets`/`spill_sets` tables and the live-in `entry_fill` set are all
/// derived over the range the allocator was handed. A sub-block is a *fresh native
/// function* whose prologue loads `entry_fill`, so reusing the containing block's
/// allocation would (a) leave a cached register the block first *writes* (before `start`)
/// unloaded, and a read of it inside the sub-range would then take whatever the previous
/// native code left in that machine register instead of its `machine.x` value, and
/// (b) carry the dirty bits accumulated at pcs `< start`, so the sub-block's first spill
/// would store such a leftover word into `machine.x`. Deriving the allocation from
/// `[start, end)` makes the sub-block's cache self-contained, exactly like a lazy one.
///
/// Everything else matches the lazy pass: a `start` at or past the end of the function, one
/// that is already a compiled block start (`block_labels`), or one no IR block strictly
/// contains, is skipped; each sub-block's range is capped at the next continuation address
/// ([`sub_block_end`]), so — exactly like the lazy pass — the emitted ranges are disjoint and
/// the total lowered length stays O(n). Like the lazy version a sub-block reserves no entry
/// label and adds no `entries` slot, so `block_count`/`code_map`/`code_address_range` keep
/// describing IR blocks only.
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
        let end = sub_block_end(code, block, start);
        if start >= end {
            continue;
        }
        // The **emitted range's** own allocation (see the doc comment): the sub-block is a
        // fresh native function, so its cache, its live-in loads and its dirty sets must
        // describe `[start, end)` — the containing block's allocation is indexed from
        // `block.start` and is wrong for it. The touches go to `discard`, so the
        // whole-module summary (built from the IR blocks' own allocations) is unchanged.
        let allocation = allocate_registers(module, code, start, end);
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
/// setup, the position-store helper call ([`emit_position_store`](super::emit_position_store) — the same
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
/// [`code_capacity_estimate`]): the prologue (the frame-register pushes, the
/// `frame_slot_len`-byte `rt`-slot reservation, the `rbx` load, the
/// [`emit_position_store`](super::emit_position_store) helper call)
/// and the epilogue (the frame teardown and `ret`) plus their padding.
const BLOCK_BYTES: usize = 192;

/// The code-size budget reserved for one instruction (see
/// [`code_capacity_estimate`]).
///
/// Every instruction the block does not lower natively is emitted as the full
/// [`emit_step_and_continue`](super::emit_step_and_continue) sequence — the exit spill, the three helper arguments,
/// the indirect call, the continuation check and the reload ([`fill`](super::fill)) — which is
/// the largest per-instruction lowering the emitter produces (~150 bytes with five
/// cached registers). Budgeting for the deopt form for *every* instruction also
/// covers the inlined forms, which are smaller.
const INSTRUCTION_BYTES: usize = 192;

/// The code-size budget reserved for one `gc_bif` (see
/// [`code_capacity_estimate`]): the guarded small-int fast path (~140 bytes with
/// `machine.x` operand loads and a `machine.x` result store, ~150 for the
/// `erlang:'*'/2` sequence, which adds the untag/re-tag shifts and a second
/// constant compare) plus the [`emit_step_and_continue`](super::emit_step_and_continue) fallback it branches to
/// (~150 bytes).
const FAST_PATH_BYTES: usize = 384;

/// The code-size budget reserved for one comparison BIF (see
/// [`code_capacity_estimate`]): the two operand loads (~20 bytes), the two small-int
/// guards with their mask materialisation (~40), the compare/branch (~10), both
/// `true`/`false` materialisations with their skip jumps (~45) and the
/// [`emit_step_and_continue`](super::emit_step_and_continue) fallback it branches to (~150, also covered by the
/// per-instruction [`INSTRUCTION_BYTES`]). Deliberately loose: under-estimating
/// would turn a large module into a `SystemLimit` compile error.
const COMPARE_PATH_BYTES: usize = 256;

/// The code-size budget reserved for one `get_list`/`get_hd`/`get_tl` (see
/// [`code_capacity_estimate`]): the guarded cons-cell read — a mask/test/branch, the
/// pointer computation and the CAR/CDR loads with their stores to cached or
/// `machine.x` destinations (~90 bytes) — emitted *in front of* the
/// [`emit_step_and_continue`](super::emit_step_and_continue) fallback it may still branch to, which the
/// per-instruction [`INSTRUCTION_BYTES`] already budgets.
const LIST_PATH_BYTES: usize = 128;

/// The code-size budget reserved for one `get_tuple_element` — and one
/// `select_tuple_arity` *alternative* (see [`code_capacity_estimate`]): the guarded
/// header read (the boxed test, the pointer computation, the arityval test and the
/// range compare, ~70 bytes) plus — for the arity switch — one `mov_ri`/`cmp`/`jcc`
/// and its spill-and-return stub (~45 bytes each). It is emitted *in front of* the
/// [`emit_step_and_continue`](super::emit_step_and_continue) fallback the guard may still branch to, which the
/// per-instruction [`INSTRUCTION_BYTES`] already budgets.
const TUPLE_PATH_BYTES: usize = 160;

#[cfg(test)]
mod tests {
    //! The continuation-sub-block pass's **range cut** is the one structural property
    //! this file adds that a caller can observe in the emitted bytes: the sub-block
    //! ranges must be **disjoint** (linear total code) rather than the overlapping
    //! `[p, block.end)` tails (quadratic total code) they used to be. These tests pin
    //! the scaling on **both** layouts and pin that the disjoint sub-blocks still run
    //! the whole body — the truncated tail of a frame-preserving call is reached
    //! through the **next** continuation address's own sub-block entry, which the
    //! cross-module run below exercises at every call.

    use super::*;
    use crate::jit::Tier;
    use crate::machine::{Machine, RunOutcome};
    use crate::MinimalContext;
    use beam_loader::{Export, Function, Import, ImportRef, Label, NO_FAIL, Operand};
    use beam_term::{make_small, Atom};

    /// Build a one-function module named `name` whose export `function/arity` has a
    /// **single straight-line body block**: `Label(1)` · `Label(2)` · `body…` ·
    /// `Return`. `Label(1)` and `Label(2)` are the only labels, so `Label(1)` is a
    /// one-instruction block `[0, 1)` and everything from `Label(2)` to the `Return`
    /// is one IR block `[1, code.len())` — the shape in which every frame-preserving
    /// call after the first leaves a continuation address **strictly inside** the
    /// block.
    fn body_module(
        name: &str,
        function: &str,
        arity: u32,
        imports: &[(&str, &str, u32)],
        body: Vec<Instr>,
    ) -> (Module, Atom) {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        for (import_module, import_function, import_arity) in imports {
            let module_atom = module.atom_table.intern(import_module);
            let function_atom = module.atom_table.intern(import_function);
            module.imports.push(Import {
                module: module_atom,
                function: function_atom,
                arity: *import_arity,
            });
        }

        let mut code = vec![Instr::Label(Label(1)), Instr::Label(Label(2))];
        code.extend(body);
        code.push(Instr::Return);
        let code_len = code.len();
        let blocks = vec![
            Block {
                label: Label(1),
                start: 0,
                end: 1,
            },
            // The single body block: every frame-preserving call lives inside it.
            Block {
                label: Label(2),
                start: 1,
                end: code_len,
            },
        ];
        let labels = BTreeMap::from([(Label(1), 0), (Label(2), 1)]);

        let name_atom = module.atom_table.intern(function);
        module.functions.push(Function {
            func_label: Label(1),
            entry_label: Label(2),
            module: module.name,
            name: name_atom,
            arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: name_atom,
            arity,
            label: Label(2),
        });
        (module, name_atom)
    }

    /// A module `f/0` whose body block is `rounds` consecutive **non-tail `call_ext`s**
    /// to `erlang:'+'/2`: `x(0)` accumulates (starting at `0`), `x(1)` is the constant
    /// `1`. This is the call-dense straight-line shape — the maximum number of
    /// continuation addresses one IR block can hold — so it is the worst case for the
    /// sub-block pass.
    fn call_dense_module(rounds: usize) -> Module {
        let mut body = vec![
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(1),
            },
        ];
        for _ in 0..rounds {
            body.push(Instr::CallExt {
                arity: 2,
                target: ImportRef(0),
            });
        }
        body_module("call_dense", "f", 0, &[("erlang", "+", 2)], body).0
    }

    /// The emitted byte total of the **eager** (whole-module) artifact for a body of
    /// `rounds` frame-preserving calls.
    fn eager_code_len(rounds: usize) -> usize {
        compile_module(&call_dense_module(rounds))
            .expect("the call-dense module compiles")
            .code_len()
    }

    /// The emitted byte total of the **lazy** (per-function) artifact for the same shape.
    fn lazy_code_len(rounds: usize) -> usize {
        let module = call_dense_module(rounds);
        let labels = interpreter::build_label_map(&module);
        compile_function(&module, &labels, 0)
            .expect("the call-dense module compiles")
            .buffer
            .len()
    }

    /// The continuation sub-blocks must lower **disjoint** ranges, so the emitted code
    /// grows **linearly** with the number of frame-preserving calls, not quadratically.
    ///
    /// Before the range cut each of the `n` continuation addresses got a sub-block
    /// covering `[p, block.end)`, so the block tail was re-emitted once per call —
    /// `~n²/2` instructions, ~16× the code at `4n` calls versus `n`. With each range
    /// capped at the next continuation address the ranges tile the block and the total
    /// is O(n), ~4× at `4n` versus `n`. Both layouts are pinned.
    #[test]
    fn the_continuation_sub_blocks_scale_linearly_not_quadratically() {
        let dense = 64usize;
        for (layout, len) in [
            ("eager", eager_code_len as fn(usize) -> usize),
            ("lazy", lazy_code_len as fn(usize) -> usize),
        ] {
            let small = len(dense);
            let large = len(4 * dense);
            let ratio = large as f64 / small as f64;
            assert!(
                small > 0 && ratio < 6.0,
                "{layout}: {} calls cost {large} bytes vs {small} at {dense} — {ratio:.2}× \
                 (disjoint ranges are ~4×, overlapping tails ~16×), so the continuation \
                 sub-blocks are still O(n²)",
                4 * dense,
            );
            // The per-step increment must not grow as the body grows: a quadratic term
            // would roughly double it each time the call count doubles.
            let first = len(2 * dense) - len(dense);
            let second = len(4 * dense) - len(2 * dense);
            assert!(
                second < 3 * first,
                "{layout}: the +{dense}-call increment grew from {first} to {second} bytes \
                 (a linear emitter keeps it roughly constant; a quadratic one doubles it)",
            );
        }
    }

    /// `f/0`'s body block is `rounds` consecutive non-tail `call_ext`s into
    /// `callee:inc/1`.
    ///
    /// The body opens with `allocate 0` and closes with `deallocate 0` before the
    /// `return`: a frame-preserving call writes its continuation into the **live**
    /// frame's continuation slot, so a body that made non-tail calls without owning a
    /// frame would leave its own `return` reading the last call's continuation (an
    /// infinite loop). The `deallocate` restores the bottom frame's `Halt`.
    fn caller_module(rounds: usize) -> (Module, Atom) {
        let mut body = vec![
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(0),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
        ];
        for _ in 0..rounds {
            body.push(Instr::CallExt {
                arity: 1,
                target: ImportRef(0),
            });
        }
        body.push(Instr::Deallocate(0));
        body_module("caller", "f", 0, &[("callee", "inc", 1)], body)
    }

    /// `callee:inc/1` — `x(0) := x(0) + 1`.
    fn callee_module() -> (Module, Atom) {
        let body = vec![Instr::Bif {
            arity: 2,
            bif: ImportRef(0),
            fail: NO_FAIL,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        }];
        body_module("callee", "inc", 1, &[("erlang", "+", 2)], body)
    }

    /// The disjoint sub-blocks must still execute **every** call: a host that resolves
    /// the `call_ext` into the same machine (`Step::Enter`) makes the driver re-enter
    /// at each call's continuation address, which is exactly a truncated sub-block's
    /// entry. `rounds` calls that each add one must therefore accumulate to `rounds`,
    /// on both tiers — a cut that dropped the tail of a call before its effect could
    /// not produce this value.
    #[test]
    fn the_disjoint_sub_blocks_still_run_every_call_ext() {
        const ROUNDS: usize = 8;
        let (caller, name) = caller_module(ROUNDS);
        let (callee, _) = callee_module();
        let callee = std::sync::Arc::new(callee);

        for tier in [Tier::Interpreter, Tier::BaselineJit] {
            let mut ctx = MinimalContext::new();
            // The reference `erlang:'+'/2` the callee's `Bif` reaches synchronously.
            ctx.set_bif_hook(crate::jit::bench::support::arith_hook(None, None));
            let target = std::sync::Arc::clone(&callee);
            ctx.set_external_resolver(Box::new(move |module, function, arity| {
                if module == "callee" && function == "inc" && arity == 1 {
                    // The callee's entry label (the label after its `func_info`).
                    Ok(Some((std::sync::Arc::clone(&target), Label(2))))
                } else {
                    Ok(None)
                }
            }));

            let mut machine = Machine::new();
            let outcome = machine
                .run_export_outcome_with_tier(&caller, name, 0, &[], &mut ctx, tier)
                .unwrap_or_else(|err| panic!("{tier:?}: the cross-module run returns: {err:?}"));
            assert_eq!(
                outcome,
                RunOutcome::Returned(make_small(ROUNDS as i64)),
                "{tier:?}: {ROUNDS} resolved `callee:inc/1` calls accumulate to {ROUNDS} \
                 — a truncated sub-block that skipped a call before its effect could not"
            );
        }
    }
}
