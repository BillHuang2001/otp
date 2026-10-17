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

//! The pc-block CFG code generator: typed IR -> Cranelift IR -> native code.
//!
//! One Cranelift function per IR [`beam_loader::Function`], one Cranelift basic
//! block per reachable `pc`, plus the shared driver-facing return block and the
//! shared per-function continuation-dispatch block. Every instruction the tier
//! does not inline is executed by the shared interpreter-backed helper
//! (`jit_step_one`), so the tier's results are interpreter-identical by
//! construction. See the parent module docs for the full contract.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;
use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Function, Instr, Module};
use beam_term::Eterm;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, FuncRef, InstBuilder, Signature, UserFuncName, Value,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module as ClModule};

use crate::jit::baseline::{encode_target, jit_step_one};

use super::compare::{emit_inline_comparison, inline_comparison};
use super::inline::{
    emit_inline_arith, emit_inline_move, emit_inline_swap, emit_inline_test, fast_arith,
    inline_move, inline_swap, inline_test,
};
use super::module_error;
use super::report::CodegenReport;
use super::shape::{
    emit_inline_get_tuple_element, emit_inline_list_read, emit_inline_select_tuple_arity,
    emit_inline_select_val, inline_get_tuple_element, inline_list_read, inline_select_tuple_arity,
    inline_select_val,
};
use super::CompiledFunctions;

/// The name the per-instruction helper ([`jit_step_one`]) is registered under in
/// the JIT module's symbol table.
pub(super) const HELPER_SYMBOL: &str = "jit_step_one";

/// The prefix of the symbol every compiled IR function is defined under.
///
/// The function index is appended (e.g. `beam_func_3`), which makes the names
/// unique within a module and legible in a disassembler.
pub(super) const FUNCTION_PREFIX: &str = "beam_func_";

/// The native signature of a compiled Cranelift function: the `i64` runtime pointer
/// in, the entry `pc` the caller wants to start at, and the block-ABI control word
/// out.
///
/// This is the Rust-side view of the Cranelift signature `(I64, I64) -> I64`; the two
/// must stay in step. The second `i64` is an **entry `pc`**: the function's prologue
/// dispatches on it (see [`emit_pc_entry_dispatch`]) over the same candidate set the
/// helper-word continuation chain resolves against ([`candidate_pcs`]), so a compiled
/// function can be natively entered at any IR block start or non-tail-call
/// continuation, not only at its entry `pc`.
pub(super) type NativeFunc = extern "C" fn(i64, i64) -> i64;

/// The byte offset of `JitRuntime::x_regs` inside the runtime block (the `8` the
/// block ABI documents on a 64-bit target).
///
/// The authoritative constant lives with the block ABI
/// (`baseline::runtime::RUNTIME_XREGS_DISP`), but that module is private to
/// `baseline`. `x_regs` is the pointer field right after `machine` in a
/// `#[repr(C)]` block, so the offset is exactly one pointer width.
pub(super) const XREGS_DISP: i32 = mem::size_of::<*const u8>() as i32;

/// The byte offset of the `machine.x` slot an x-register index names (an `Eterm` is
/// one machine word).
pub(super) fn x_offset(x: u32) -> i32 {
    (x as i32) * (mem::size_of::<Eterm>() as i32)
}

/// Compile every IR function of `module` into one Cranelift function.
///
/// Functions without code (or without a resolvable entry label) are skipped: the
/// driver simply keeps interpreting them, which is always correct.
pub(super) fn compile_module(
    module: &Module,
    builder_context: &mut FunctionBuilderContext,
) -> Result<CompiledFunctions> {
    let mut builder = JITBuilder::new(default_libcall_names()).map_err(module_error)?;
    // Register the shared per-instruction helper under its symbol name so the JIT
    // resolves the import against this very process (no dlsym guesswork).
    builder.symbol(HELPER_SYMBOL, jit_step_one as *const () as *const u8);
    let mut jit = JITModule::new(builder);

    // The helper's signature: `(rt: i64, func: i32, pc: i32) -> i64`.
    let mut helper_sig = jit.make_signature();
    helper_sig.params.push(AbiParam::new(types::I64));
    helper_sig.params.push(AbiParam::new(types::I32));
    helper_sig.params.push(AbiParam::new(types::I32));
    helper_sig.returns.push(AbiParam::new(types::I64));
    let helper_id = jit
        .declare_function(HELPER_SYMBOL, Linkage::Import, &helper_sig)
        .map_err(module_error)?;

    // One shared signature per IR function: `(rt: i64, pc: i64) -> i64`. The second
    // parameter is the entry `pc` the caller wants to start at, dispatched in the
    // function's prologue (see `emit_pc_entry_dispatch`).
    let mut function_sig = jit.make_signature();
    function_sig.params.push(AbiParam::new(types::I64));
    function_sig.params.push(AbiParam::new(types::I64));
    function_sig.returns.push(AbiParam::new(types::I64));

    // The module-wide `Label -> (func, pc)` map an inlined `jump` resolves its
    // target through.
    let labels = crate::interpreter::build_label_map(module);

    let mut entry_pc: HashMap<u32, u32> = HashMap::new();
    let mut enter_pcs: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut defined: Vec<(u32, FuncId)> = Vec::new();
    let mut report = CodegenReport::default();
    for (index, function) in module.functions.iter().enumerate() {
        if function.code.is_empty() {
            continue;
        }
        let Some(&pc) = function.labels.get(&function.entry_label) else {
            continue;
        };
        let func_index = index as u32;
        let name = format!("{FUNCTION_PREFIX}{func_index}");
        let id = jit
            .declare_function(&name, Linkage::Export, &function_sig)
            .map_err(module_error)?;
        // The pcs the compiled function may be **natively entered** at: the IR block
        // starts plus every non-tail-call continuation. A pc must have a real
        // instruction to start a block for (`< code.len()`), so a candidate at or past
        // the end of the code is dropped — exactly the set [`emit_pc_entry_dispatch`]
        // can dispatch to, so [`CompiledFunctions::is_enterable`] never claims an
        // address the prologue would send to its defensive fallback. The entry label's
        // `pc` is always added back, so a function is always enterable at its entry
        // (the pre-existing behaviour) even for odd input.
        let code_len = function.code.len();
        let mut candidates = candidate_pcs(function);
        candidates.retain(|&candidate| (candidate as usize) < code_len);
        if !candidates.contains(&(pc as u32)) {
            candidates.push(pc as u32);
            candidates.sort_unstable();
            candidates.dedup();
        }
        emit_function(
            &mut jit,
            builder_context,
            helper_id,
            &function_sig,
            id,
            module,
            func_index,
            &labels,
            function,
            pc as u32,
            &candidates,
            &mut report,
        )?;
        entry_pc.insert(func_index, pc as u32);
        enter_pcs.insert(func_index, candidates);
        defined.push((func_index, id));
    }

    // Link the module: resolve the helper import, patch relocations and flip the
    // code memory to read+execute.
    jit.finalize_definitions().map_err(module_error)?;

    let mut entries: HashMap<u32, *const u8> = HashMap::new();
    for (func_index, id) in defined {
        entries.insert(func_index, jit.get_finalized_function(id));
    }

    Ok(CompiledFunctions {
        _module: jit,
        entries,
        entry_pc,
        enter_pcs,
        highest_native_register: report.highest_native_register,
        inlined_tests: report.inlined_tests,
        inlined_gc_bifs: report.inlined_gc_bifs,
        inlined_comparisons: report.inlined_comparisons,
        inlined_select_vals: report.inlined_select_vals,
        inlined_tuple_reads: report.inlined_tuple_reads,
        inlined_list_reads: report.inlined_list_reads,
        inlined_swaps: report.inlined_swaps,
        inlined_tail_calls: report.inlined_tail_calls,
    })
}
/// Emit one cranelift function for IR function `function` (`func_index` of
/// `module`), entering at `entry_pc` and able to be entered across `candidates`; see
/// the module docs for the pc-block CFG.
#[allow(clippy::too_many_arguments)]
fn emit_function(
    jit: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    helper_id: FuncId,
    signature: &Signature,
    id: FuncId,
    module: &Module,
    func_index: u32,
    labels: &LabelMap,
    function: &Function,
    entry_pc: u32,
    candidates: &[u32],
    report: &mut CodegenReport,
) -> Result<()> {
    let code = &function.code;
    let code_len = code.len();
    let mut ctx = jit.make_context();
    ctx.func.signature = signature.clone();
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, builder_context);
        let helper = jit.declare_func_in_func(helper_id, &mut builder.func);

        // --- entry: dispatch on the entry `pc` argument. -----------------------
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.append_block_params_for_function_params(entry);
        let rt = builder.block_params(entry)[0];
        let pc_param = builder.block_params(entry)[1];
        let this_func = builder.ins().iconst(types::I32, i64::from(func_index));

        let mut cfg = Cfg {
            pc_blocks: HashMap::new(),
            pending: Vec::new(),
            emitted: HashSet::new(),
            leave: None,
            dispatch: None,
            dispatch_emitted: false,
        };
        emit_pc_entry_dispatch(&mut builder, &mut cfg, code_len, pc_param, entry_pc, candidates);

        // --- one round per reachable `pc`, in worklist order. ------------------
        //
        // The worklist drains first, then the shared dispatch chain is emitted (it
        // may queue further `pc` blocks), which are drained in turn: a block is only
        // created the first time something transfers to it, so a `pc` nothing can
        // reach costs nothing at all.
        loop {
            while let Some(pc) = cfg.pending.pop() {
                if !cfg.emitted.insert(pc) {
                    continue;
                }
                let Some(block) = cfg.pc_blocks.get(&pc).copied() else {
                    continue;
                };
                builder.switch_to_block(block);
                emit_pc(
                    &mut builder,
                    &mut cfg,
                    pc,
                    code_len,
                    code,
                    module,
                    labels,
                    func_index,
                    helper,
                    rt,
                    this_func,
                    report,
                );
            }
            // The shared dispatch chain is filled once per function, after every
            // `pc` block reached so far (see `emit_dispatch_chain`).
            match cfg.dispatch {
                Some(dispatch) if !cfg.dispatch_emitted => {
                    cfg.dispatch_emitted = true;
                    builder.switch_to_block(dispatch);
                    emit_dispatch_chain(&mut builder, &mut cfg, code_len, candidates);
                }
                _ => break,
            }
        }

        // --- the shared driver-facing return, if a helper block needed it. -----
        if let Some(leave) = cfg.leave {
            builder.switch_to_block(leave);
            let word = builder.block_params(leave)[0];
            builder.ins().return_(&[word]);
        }
        builder.seal_all_blocks();
        builder.finalize();
    }
    jit.define_function(id, &mut ctx).map_err(module_error)?;
    jit.clear_context(&mut ctx);
    Ok(())
}
/// The mutable compile-time state of one function's pc-block CFG.
pub(super) struct Cfg {
    /// The Cranelift block for each `pc` the CFG has created so far.
    pc_blocks: HashMap<u32, Block>,
    /// The pcs still to emit: a block is created (and queued) the first time
    /// something transfers to it, and emitted exactly once, when it is popped.
    pending: Vec<u32>,
    /// The pcs already emitted, so a `pc` reached from many predecessors is filled
    /// once.
    emitted: HashSet<u32>,
    /// The shared "return the block-ABI word to the driver" block, created on the
    /// first helper dispatch (it takes the word as its single block parameter).
    leave: Option<Block>,
    /// The shared per-function continuation-dispatch block, created on the first
    /// helper dispatch (it takes the block-ABI word as its single block parameter).
    ///
    /// There is **one** such chain per function, not one per helper call site: every
    /// helper site jumps here with its word, so the chain's cost is linear in the
    /// candidate set rather than in `candidates × helper sites` (which would be
    /// quadratic — a large function's candidate set is itself O(code length)).
    dispatch: Option<Block>,
    /// Whether the shared dispatch block's body has been filled.
    dispatch_emitted: bool,
}
/// The Cranelift block for `pc`, created (and queued for emission) on first use.
fn block_for(builder: &mut FunctionBuilder, cfg: &mut Cfg, pc: u32) -> Block {
    if let Some(&block) = cfg.pc_blocks.get(&pc) {
        return block;
    }
    let block = builder.create_block();
    cfg.pc_blocks.insert(pc, block);
    cfg.pending.push(pc);
    block
}

/// The shared "return the block-ABI word to the driver" block (created on first
/// use, with its single `i64` parameter appended once).
fn leave_block(builder: &mut FunctionBuilder, cfg: &mut Cfg) -> Block {
    if let Some(block) = cfg.leave {
        return block;
    }
    let block = builder.create_block();
    builder.append_block_param(block, types::I64);
    cfg.leave = Some(block);
    block
}

/// The shared per-function continuation-dispatch block (created on first use, with
/// its single `i64` block parameter — the helper's word — appended once).
fn dispatch_block(builder: &mut FunctionBuilder, cfg: &mut Cfg) -> Block {
    if let Some(block) = cfg.dispatch {
        return block;
    }
    let block = builder.create_block();
    builder.append_block_param(block, types::I64);
    cfg.dispatch = Some(block);
    block
}

/// Transfer control to the block for `pc`, creating it if this is its first mention.
fn goto_pc(builder: &mut FunctionBuilder, cfg: &mut Cfg, pc: u32) {
    let block = block_for(builder, cfg, pc);
    builder.ins().jump(block, &[]);
}

/// Continue at `pc`: jump to its block when `pc` is a real instruction of this
/// function, else hand the encoded `(func_index, pc)` word to the driver.
///
/// `pc` is out of range only when an instruction falls through past the end of the
/// code (an unmodelled/odd shape — every compiler-emitted function body ends in a
/// `return`). The driver then takes the interpreter's exact step at that address, so
/// the observable behaviour is the interpreter's; creating a block for an ever-larger
/// `pc` instead would extend the CFG without bound, which is exactly the bug a
/// straight-line fall-through past the end used to cause.
pub(super) fn goto_or_leave(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    code_len: usize,
    func_index: u32,
    pc: u32,
) {
    if (pc as usize) < code_len {
        goto_pc(builder, cfg, pc);
    } else {
        let word = builder
            .ins()
            .iconst(types::I64, encode_target(func_index, pc));
        builder.ins().return_(&[word]);
    }
}
/// Emit the body of the block for `pc` — the inlined instruction followed by a
/// transfer, or the helper call plus its dispatch (see the module docs).
#[allow(clippy::too_many_arguments)]
fn emit_pc(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    code: &[Instr],
    module: &Module,
    labels: &LabelMap,
    func_index: u32,
    helper: FuncRef,
    rt: Value,
    this_func: Value,
    report: &mut CodegenReport,
) {
    match code.get(pc as usize) {
        // Metadata no-ops (`ops.tab:103-108`): they never change control flow, so the
        // block falls through. `func_info` is **not** here: it is the failed-clause
        // raise (the interpreter's `Instr::FuncInfo` arm), so inlining it as a no-op
        // would fall through to the entry label and re-run the clause tests forever.
        Some(
            Instr::Label(_)
            | Instr::Line(_)
            | Instr::IntCodeEnd
            | Instr::ExecutableLine { .. }
            | Instr::DebugLine { .. },
        ) => {
            goto_or_leave(builder, cfg, code_len, func_index, pc + 1);
            return;
        }
        Some(Instr::Move { src, dst }) => {
            if let Some((dst_x, source)) = inline_move(src, dst) {
                emit_inline_move(builder, rt, dst_x, &source, report);
                goto_or_leave(builder, cfg, code_len, func_index, pc + 1);
                return;
            }
        }
        Some(Instr::Swap { a, b }) => {
            if let Some((a_x, b_x)) = inline_swap(a, b) {
                report.inlined_swap();
                emit_inline_swap(builder, rt, a_x, b_x, report);
                goto_or_leave(builder, cfg, code_len, func_index, pc + 1);
                return;
            }
        }
        Some(Instr::Jump { target }) => {
            if let Ok(resolved) = labels.resolve(*target) {
                emit_target_branch(builder, cfg, code_len, func_index, resolved);
                return;
            }
            // An undefined label keeps the interpreter's exact `unknown_label` error:
            // hand the jump to the helper rather than inventing one.
        }
        // A local tail call **without** a frame to pop (`call_only Arity Label`):
        // `i_call_only(Dest)` is `$DISPATCH_REL(Dest)`, so it transfers control exactly
        // like a `jump` — the same lowering the baseline tier gives it. (`call_last`
        // must first pop the frame, which is Rust-side `Machine` state, so it stays on
        // the helper; a target in another IR function, or an undefined label, deopts
        // the same way a `jump` does.)
        Some(Instr::CallOnly { target, .. }) => {
            if let Ok(resolved) = labels.resolve(*target) {
                report.inlined_tail_call();
                emit_target_branch(builder, cfg, code_len, func_index, resolved);
                return;
            }
        }
        // The inlined word-level `test` subset: the type tests and the provably-exact
        // comparison shapes (see [`inline::inline_test`]). Everything outside that subset, and
        // every run-time guard failure inside it, deopts to the helper, whose
        // interpreter step is the test's exact semantics (including the `fail`-label /
        // raise split).
        Some(Instr::Test { kind, fail, args }) => {
            if let Some(plan) = inline_test(module, *kind, *fail, args, labels) {
                report.inlined_test(*kind);
                emit_inline_test(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        // The BIFs the tier inlines: the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` fast
        // path first (a *shape* test, never a value test), then the eight comparison
        // BIFs (see [`compare`]). Every mismatch, and every run-time guard failure,
        // falls through to the helper, whose interpreter step is the BIF's exact
        // semantics (including the `fail`-label / raise split).
        Some(gc_bif @ Instr::GcBif { .. }) => {
            if let Some(fast) = fast_arith(module, gc_bif) {
                report.inlined_gc_bif(fast.op);
                emit_inline_arith(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &fast,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
            if let Some(plan) = inline_comparison(module, gc_bif) {
                report.inlined_comparison(plan.op.name());
                emit_inline_comparison(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        // The comparison BIFs (`erlang`'s `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`),
        // which the compiler emits as a **non-collecting** `bif` (see [`compare`]).
        // Any other BIF, an un-addressable operand, or a module without both
        // `true`/`false` interned falls through to the helper.
        Some(bif @ Instr::Bif { .. }) => {
            if let Some(plan) = inline_comparison(module, bif) {
                report.inlined_comparison(plan.op.name());
                emit_inline_comparison(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        // The exact-value switch (`select_val`): the immediate-alternatives-only
        // subset (see [`shape::inline_select_val`]) — a chain of word compares with no
        // run-time guard, because `=:=` over immediates *is* a word compare. Every
        // shape above the subset stays on the helper (including a `NO_FAIL` switch,
        // which raises, and any non-immediate alternative).
        Some(Instr::SelectVal { value, fail, pairs }) => {
            if let Some(plan) = inline_select_val(value, *fail, pairs, labels) {
                report.inlined_select_val();
                emit_inline_select_val(builder, cfg, code_len, func_index, rt, &plan, report);
                return;
            }
        }
        // The guarded tuple reads: `get_tuple_element` and `select_tuple_arity`
        // inline only for a run-time tuple (see [`shape::inline_get_tuple_element`] /
        // [`shape::inline_select_tuple_arity`]); every rejection — a non-tuple source, an
        // out-of-range element, a `NO_FAIL` arity switch — deopts to the helper, whose
        // interpreter step is the family's exact semantics (including the
        // `{badmatch, V}` raise both reads share).
        Some(Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        }) => {
            if let Some(plan) = inline_select_tuple_arity(tuple, *fail, destinations, labels) {
                report.inlined_tuple_read("select_tuple_arity");
                emit_inline_select_tuple_arity(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        Some(Instr::GetTupleElement {
            source,
            element,
            dst,
        }) => {
            if let Some(plan) = inline_get_tuple_element(source, *element, dst) {
                report.inlined_tuple_read("get_tuple_element");
                emit_inline_get_tuple_element(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        // The guarded list reads (`get_list` / `get_hd` / `get_tl`): inline only for a
        // run-time cons cell (see [`shape::inline_list_read`]); a non-cons source (`[]`, an
        // integer, a tuple, …) deopts to the helper, whose interpreter step raises the
        // family's exact `{badmatch, Source}`.
        Some(Instr::GetList { source, head, tail }) => {
            if let Some(plan) = inline_list_read(source, Some(head), Some(tail)) {
                report.inlined_list_read("get_list");
                emit_inline_list_read(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        Some(Instr::GetHd { source, head }) => {
            if let Some(plan) = inline_list_read(source, Some(head), None) {
                report.inlined_list_read("get_hd");
                emit_inline_list_read(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        Some(Instr::GetTl { source, tail }) => {
            if let Some(plan) = inline_list_read(source, None, Some(tail)) {
                report.inlined_list_read("get_tl");
                emit_inline_list_read(
                    builder,
                    cfg,
                    pc,
                    code_len,
                    func_index,
                    rt,
                    &plan,
                    helper,
                    this_func,
                    report,
                );
                return;
            }
        }
        _ => {}
    }
    // Everything else: one interpreter step, then dispatch on the word it returned.
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
}
/// Emit a branch to a resolved `(func, pc)` target: a native jump when it is an
/// address **this** function has code for, else the encoded `(func, pc)` word back
/// to the driver (which continues in that function, or takes the interpreter step).
///
/// This is the one place a `jump` and a `test`'s `fail` branch turn a resolved
/// [`Label`] into control flow, so both stay bit-identical.
pub(super) fn emit_target_branch(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    code_len: usize,
    func_index: u32,
    target: (u32, u32),
) {
    let (target_func, target_pc) = target;
    if target_func == func_index && (target_pc as usize) < code_len {
        let block = block_for(builder, cfg, target_pc);
        builder.ins().jump(block, &[]);
    } else {
        // A target in another IR function (or an address this function has no code
        // for) leaves this native function: hand the encoded word to the driver.
        let word = builder
            .ins()
            .iconst(types::I64, encode_target(target_func, target_pc));
        builder.ins().return_(&[word]);
    }
}
/// Emit the helper call for `(func_index, pc)` and transfer control on its result.
///
/// `word = jit_step_one(rt, this_func, pc)` is folded exactly like the driver folds
/// a block's return value:
///
/// * a negative sentinel (`HALT`/`ERROR`), or a target in a **different** IR
///   function, is handed back to the driver unchanged;
/// * a same-function target `pc + 1` (the helper's `Step::Next`, its most common
///   result) continues straight to that `pc`'s block — created on demand;
/// * every other same-function target goes to the **shared** per-function
///   continuation-dispatch block, which resolves it against the CFG's candidate set
///   and hands it back to the driver when it is not one of them (the driver's
///   interpreter step for that address is exact).
pub(super) fn emit_helper_dispatch(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    helper: FuncRef,
    rt: Value,
    this_func: Value,
) {
    let pc_const = builder.ins().iconst(types::I32, i64::from(pc));
    let call = builder.ins().call(helper, &[rt, this_func, pc_const]);
    let word = builder.inst_results(call)[0];

    let leave = leave_block(builder, cfg);
    let proceed = builder.create_block();
    // `HALT` (`-1`) / `ERROR` (`-2`) are the only negative words a step returns.
    let sentinel = builder.ins().icmp_imm(IntCC::SignedLessThan, word, 0);
    builder.ins().brif(sentinel, leave, &[word], proceed, &[]);

    // `(func, pc)` = `(word >> 32, word & 0xFFFF_FFFF)`.
    builder.switch_to_block(proceed);
    let high = builder.ins().ushr_imm(word, 32);
    let target_func = builder.ins().ireduce(types::I32, high);
    let elsewhere = builder
        .ins()
        .icmp(IntCC::NotEqual, target_func, this_func);
    let same_func = builder.create_block();
    builder.ins().brif(elsewhere, leave, &[word], same_func, &[]);

    builder.switch_to_block(same_func);
    let dispatch = dispatch_block(builder, cfg);
    // `Step::Next` — the helper's most common result — continues at `pc + 1` and is
    // *not* always a label position, so it gets a one-comparison fast path.
    if ((pc as usize) + 1) < code_len {
        let target_pc = builder.ins().ireduce(types::I32, word);
        let next_block = block_for(builder, cfg, pc + 1);
        let is_next = builder
            .ins()
            .icmp_imm(IntCC::Equal, target_pc, i64::from(pc + 1));
        builder.ins().brif(is_next, next_block, &[], dispatch, &[word]);
    } else {
        builder.ins().jump(dispatch, &[word]);
    }
}
/// Fill the shared per-function continuation-dispatch block.
///
/// The block's `i64` parameter is a helper word whose target function is already
/// known to be this one; the chain compares its `pc` against the CFG's candidate set
/// and jumps to the matching `pc` block — the target block is created lazily, on
/// demand. A `pc` outside the candidate set (which the set is a superset of, so
/// today there is none) is handed back to the driver, whose interpreter step for
/// that address is exact.
///
/// The current block must already be the dispatch block when this is called.
fn emit_dispatch_chain(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    code_len: usize,
    candidates: &[u32],
) {
    let Some(dispatch) = cfg.dispatch else {
        return;
    };
    let word = builder.block_params(dispatch)[0];
    let target_pc = builder.ins().ireduce(types::I32, word);

    let mut cursor = dispatch;
    let mut started = false;
    for &candidate in candidates {
        if candidate as usize >= code_len {
            continue;
        }
        if started {
            builder.switch_to_block(cursor);
        }
        started = true;
        let constant = builder.ins().iconst(types::I32, i64::from(candidate));
        let hit = builder.ins().icmp(IntCC::Equal, target_pc, constant);
        let block = block_for(builder, cfg, candidate);
        let miss = builder.create_block();
        builder.ins().brif(hit, block, &[], miss, &[]);
        cursor = miss;
    }
    if started {
        builder.switch_to_block(cursor);
    }
    // No candidate matched (or there are none): the driver takes the interpreter's
    // exact step for that address.
    builder.ins().return_(&[word]);
}
/// Emit the function-prologue **pc dispatch**: transfer control to the block for the
/// entry `pc` the compiled function was called with.
///
/// The native function's second argument is the entry `pc` the driver asked for
/// ([`CompiledEntries::enter`](super::CompiledEntries) only ever passes a member of
/// [`candidate_pcs`]), so the prologue compares it against the candidate set:
///
/// * the function's `entry_pc` is checked **first** — a one-comparison fast path, the
///   common case (the driver enters a function at its entry `pc`, and a cross-function
///   `Step::Goto`/non-tail-call return that lands in a **different** function also
///   starts a fresh native call at that function's entry `pc`);
/// * every other candidate (a same-function IR block start, or a non-tail-call
///   continuation `call_site + 1`) follows in the candidates' sorted order, so the
///   driver can natively pick up a cross-function call return;
/// * the final fallback jumps to `block_for(entry_pc)` — **defensive and unreachable**,
///   because `enter` guarantees membership, so a `pc` outside the set never arrives.
///
/// This is deliberately separate from [`emit_dispatch_chain`]: that chain folds a word
/// a *helper step* returned, whereas this folds the entry-`pc` *argument*. They share
/// the candidate set but neither shares the other's mechanism, so the helper-word
/// chain keeps its existing behaviour byte-for-byte.
///
/// The current block must already be the function's entry block when this is called.
fn emit_pc_entry_dispatch(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    code_len: usize,
    pc_value: Value,
    entry_pc: u32,
    candidates: &[u32],
) {
    let pc = builder.ins().ireduce(types::I32, pc_value);
    let entry_block = block_for(builder, cfg, entry_pc);
    let is_entry = builder.ins().icmp_imm(IntCC::Equal, pc, i64::from(entry_pc));
    let mut cursor = builder.create_block();
    builder
        .ins()
        .brif(is_entry, entry_block, &[], cursor, &[]);

    for &candidate in candidates {
        if candidate == entry_pc || candidate as usize >= code_len {
            continue;
        }
        builder.switch_to_block(cursor);
        let constant = builder.ins().iconst(types::I32, i64::from(candidate));
        let hit = builder.ins().icmp(IntCC::Equal, pc, constant);
        let block = block_for(builder, cfg, candidate);
        let miss = builder.create_block();
        builder.ins().brif(hit, block, &[], miss, &[]);
        cursor = miss;
    }

    builder.switch_to_block(cursor);
    // Defensive fallback (unreachable — `enter` guarantees membership).
    builder.ins().jump(entry_block, &[]);
}
/// Every `pc` a helper step inside `function` may continue at natively.
///
/// A same-function `Step::Goto { func, pc }` is always either the label map's
/// target of *some* instruction (every branch/call/fail target in the IR is a
/// [`Label`]) or the continuation a non-tail `call` stored in the frame's CP slot,
/// which the engine sets to the call site's `pc + 1`. The label positions plus one
/// past every non-tail call site is therefore a **superset** of those
/// continuations.
fn candidate_pcs(function: &Function) -> Vec<u32> {
    let mut candidates: BTreeSet<u32> = BTreeSet::new();
    for &pc in function.labels.values() {
        candidates.insert(pc as u32);
    }
    for (index, instr) in function.code.iter().enumerate() {
        if pushes_continuation(instr) {
            candidates.insert(index as u32 + 1);
        }
    }
    candidates.into_iter().collect()
}
/// Whether `instr` stores a return continuation in the *current* frame's CP slot —
/// a non-tail call. A tail call (`call_only`/`call_last`/`call_ext_only`/…)
/// deallocates the frame first, so its successor is the caller's continuation, not
/// this function's.
fn pushes_continuation(instr: &Instr) -> bool {
    matches!(
        instr,
        Instr::Call { .. }
            | Instr::CallExt { .. }
            | Instr::CallFun { .. }
            | Instr::CallFun2 { .. }
            | Instr::Apply { .. }
    )
}
