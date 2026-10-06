//! The optional **Cranelift hot tier**: typed IR → Cranelift IR → native code.
//!
//! This is the backend behind [`Tier::Cranelift`](crate::jit::Tier::Cranelift),
//! compiled in only when the **`jit-cranelift`** Cargo feature is enabled (default
//! **OFF**, so the default build carries no Cranelift dependency and no Cranelift
//! codegen at all — Part II §F/§H keeps the hot tier off the critical path).
//!
//! # What it compiles
//!
//! For every IR [`Function`] of the module, [`compile`] builds **one** Cranelift
//! function with the System V signature
//!
//! ```text
//! extern "C" fn(rt: i64) -> i64
//! ```
//!
//! (`rt` in `rdi`, the block-ABI word in `rax`). Its body is a **pc-block CFG**:
//! one Cranelift basic block per **reachable** `pc` of the IR function, created
//! lazily by a worklist from the function's entry `pc` (so an unreachable `pc`
//! costs nothing), plus one shared "hand the block-ABI word back to the driver"
//! block. Each `pc`-block either
//!
//! 1. **inlines** the IR instruction and transfers control to a successor block —
//!    the metadata no-ops fall through to `pc + 1`, an inlinable `move` writes the
//!    destination x-register slot and falls through to `pc + 1`, an inlinable
//!    small-int `gc_bif` falls through to `pc + 1` on its fast path, and a `jump`
//!    whose label resolves **inside this IR function** jumps straight to the
//!    label's `pc` (a `jump` into a *different* IR function returns the encoded
//!    `(func, pc)` word to the driver); or
//! 2. calls the shared per-instruction helper
//!    `jit_step_one(rt, FUNC_INDEX, pc)` — the **same** interpreter-backed helper
//!    the baseline tier uses
//!    ([`baseline::jit_step_one`](crate::jit::baseline::jit_step_one) is literally
//!    [`Machine::step_instruction`](crate::Machine)) — and dispatches on the word
//!    it returned, **exactly as the driver does**: a negative sentinel
//!    (`HALT`/`ERROR`), or a target in a **different** IR function, is returned to
//!    the driver unchanged; a **same-function** target `(this_func, p)` whose `p`
//!    is one of the CFG's candidates jumps to `p`'s block.
//!
//! Control flow *within* a function is therefore native, while the semantics of
//! every single instruction stay the interpreter's — the results are
//! **interpreter-identical by construction**. This is the tier's documented
//! **cheap-deopt boundary**: every instruction the tier does not inline is executed
//! by the shared helper, and the driver falls back to one interpreter step for any
//! address no compiled function starts at.
//!
//! [`compile`]: JitBackend::compile
//!
//! # The continuation candidate set
//!
//! A helper block's `Step::Goto { func, pc }` is a **runtime** value, so the block
//! cannot statically pick a target block. A same-function continuation is always
//! either the label map's target of *some* instruction in the function (every
//! branch/call/fail target in the IR is a [`Label`]) or the return address a
//! `call` stored in the frame's CP slot — which the engine sets to `pc + 1` of the
//! call site. [`candidate_pcs`] therefore collects **the label positions plus one
//! past every non-tail call site**; the helper block compares the runtime `pc`
//! against that set and jumps to the matching block. The set is a *superset* of the
//! reachable same-function continuations, and a target outside it (there is none
//! today) is handed back to the driver, which takes the interpreter's exact step —
//! never a wrong result.
//!
//! # What it does **not** inline
//!
//! Everything outside the set above: `func_info` (the failed-clause raise, which
//! must **not** be a no-op — see the interpreter's `Instr::FuncInfo` arm), `swap`,
//! every call/return/frame instruction, every `test`/`select_*`, every BIF outside
//! the two the fast path recognises, `move` with a `Y` (or other non-x) operand or
//! an x-register index past the engine's `MAX_REGISTER` bound, and a `jump` to a
//! label the module does not define (so the interpreter's exact `unknown_label`
//! error surfaces). Each of those runs through `jit_step_one`.
//!
//! # `unsafe`
//!
//! A designated JIT unsafe zone, like [`crate::jit::baseline`]: it hands
//! [`JitRuntime`] out as a raw pointer and calls into the JIT module's executable
//! memory. The crate root keeps `#![deny(unsafe_code)]`; the module-local
//! `#![allow(unsafe_code)]` below is the third JIT exception. Every `unsafe` block
//! carries a `// SAFETY:` rationale.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem;

use beam_common::Result;
use beam_loader::{Atom, Function, Instr, Label, Module, Operand, Reg as RegRef};
use beam_term::{
    make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL, TAG_IMMED1_MASK, TAG_IMMED1_SIZE,
    TAG_IMMED1_SMALL,
};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, FuncRef, InstBuilder, MemFlags, Signature, UserFuncName, Value,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module as ClModule, ModuleError};

use crate::context::ExecutionContext;
use crate::error;
use crate::jit::baseline::{
    encode_target, jit_step_one, run_native, JitRuntime, NativeCode,
};
use crate::jit::JitBackend;
use crate::machine::{Machine, MAX_REGISTER};

/// The name the per-instruction helper ([`jit_step_one`]) is registered under in
/// the JIT module's symbol table.
const HELPER_SYMBOL: &str = "jit_step_one";

/// The prefix of the symbol every compiled IR function is defined under.
///
/// The function index is appended (e.g. `beam_func_3`), which makes the names
/// unique within a module and legible in a disassembler.
const FUNCTION_PREFIX: &str = "beam_func_";

/// The native signature of a compiled Cranelift function: one `i64` runtime
/// pointer in, the block-ABI control word out.
///
/// This is the Rust-side view of the Cranelift signature `(I64) -> I64`; the two
/// must stay in step.
type NativeFunc = extern "C" fn(i64) -> i64;

/// The byte offset of `JitRuntime::x_regs` inside the runtime block (the `8` the
/// block ABI documents on a 64-bit target).
///
/// The authoritative constant lives with the block ABI
/// (`baseline::runtime::RUNTIME_XREGS_DISP`), but that module is private to
/// `baseline`. `x_regs` is the pointer field right after `machine` in a
/// `#[repr(C)]` block, so the offset is exactly one pointer width.
const XREGS_DISP: i32 = mem::size_of::<*const u8>() as i32;

/// The byte offset of the `machine.x` slot an x-register index names (an `Eterm` is
/// one machine word).
fn x_offset(x: u32) -> i32 {
    (x as i32) * (mem::size_of::<Eterm>() as i32)
}

/// Record that x-register `x` is touched natively at `[x_regs + x*8]`.
fn touch(highest: &mut Option<u32>, x: u32) {
    *highest = Some(match *highest {
        Some(previous) => previous.max(x),
        None => x,
    });
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The optional Cranelift hot tier.
///
/// The artifact it compiles ([`CompiledFunctions`]) is caller-owned, so a module can
/// be compiled once and run many times through [`JitBackend::run_compiled`]; the
/// one-shot [`JitBackend::run_export`] (`run_with_tier` creates a fresh backend per
/// run) compiles and runs in one call. The backend owns the reusable
/// [`FunctionBuilderContext`], which Cranelift requires as scratch space while
/// lowering a function.
pub struct CraneliftJit {
    /// Scratch space reused across the module's functions (Cranelift clears it
    /// after every [`FunctionBuilder`] completes).
    builder_context: FunctionBuilderContext,
}

impl CraneliftJit {
    /// A Cranelift backend ready to compile and run a module.
    pub fn new() -> CraneliftJit {
        CraneliftJit {
            builder_context: FunctionBuilderContext::new(),
        }
    }
}

impl Default for CraneliftJit {
    fn default() -> CraneliftJit {
        CraneliftJit::new()
    }
}

impl JitBackend for CraneliftJit {
    type Compiled = CompiledFunctions;

    fn name(&self) -> &'static str {
        "jit-cranelift"
    }

    fn available(&self) -> bool {
        true
    }

    /// Compile every IR function of `module` into a Cranelift function.
    fn compile(&mut self, module: &Module) -> Result<CompiledFunctions> {
        compile_module(module, &mut self.builder_context)
    }

    /// Run exported `function/arity` through the driver loop on an already-compiled
    /// artifact.
    ///
    /// The artifact ([`CompiledFunctions`]) is caller-owned and kept alive by the
    /// caller for the call; the backend holds no per-run code state, so one artifact
    /// can be run any number of times without recompiling. [`JitBackend::run_export`]
    /// is the provided one-shot convenience over `compile` + this.
    fn run_compiled(
        &self,
        compiled: &CompiledFunctions,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let code = CompiledEntries { compiled };
        run_native(machine, module, function, arity, args, ctx, &code)
    }
}

/// The artifact [`CraneliftJit::compile`] produces: the Cranelift [`JITModule`]
/// that owns the executable mapping plus the per-function native entries.
///
/// The entries are raw pointers into the JIT module's code memory, so a
/// `CompiledFunctions` must outlive every call into it and is neither `Send` nor
/// `Sync` (the JIT module frees the mapping when dropped). The module field exists
/// to keep that mapping alive; nothing reads it after compilation.
pub struct CompiledFunctions {
    /// The JIT module whose executable mapping every entry points into.
    ///
    /// Never read after compilation: it is held only to **own** the mapping the
    /// entries point into (Cranelift frees it when the module drops), hence the
    /// leading underscore that tells the dead-code lint so.
    _module: JITModule,
    /// The function index → finalized native entry point.
    entries: HashMap<u32, *const u8>,
    /// The function index → the `pc` its compiled function starts at.
    ///
    /// A compiled function bakes its entry `pc` in, so it may only be entered at
    /// that address (the driver checks it; see [`CompiledEntries::enter`]).
    entry_pc: HashMap<u32, u32>,
    /// The highest x-register index any inlined instruction addresses directly in
    /// `machine.x`.
    ///
    /// The inlined `move` fast path reads and writes `[x_regs + x*8]` **without a
    /// bounds check**, so the driver refuses native entry when the running machine's
    /// register file is not at least this long (see [`CompiledEntries::enter`]) —
    /// the same belt-and-braces guard the baseline tier uses.
    highest_native_register: Option<u32>,
}

impl CompiledFunctions {
    /// The native entry point compiled for function index `func`, if any.
    pub fn entry(&self, func: u32) -> Option<*const u8> {
        self.entries.get(&func).copied()
    }

    /// The `pc` the compiled function for `func` starts at, if it was compiled.
    pub fn entry_pc(&self, func: u32) -> Option<u32> {
        self.entry_pc.get(&func).copied()
    }

    /// The number of compiled functions.
    pub fn function_count(&self) -> usize {
        self.entries.len()
    }

    /// The highest x-register index any inlined instruction addresses directly in
    /// `machine.x`, if any instruction was inlined.
    pub fn highest_native_register(&self) -> Option<u32> {
        self.highest_native_register
    }
}

/// The Cranelift tier's [`NativeCode`] lookup: a compiled function is entered
/// exactly at its entry `pc`.
struct CompiledEntries<'a> {
    /// The compiled functions of the module being run.
    compiled: &'a CompiledFunctions,
}

impl NativeCode for CompiledEntries<'_> {
    fn enter(&self, rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> Option<i64> {
        // The compiled function starts at the IR function's entry `pc`; entering it
        // anywhere else would run the wrong code, so every other address stays on the
        // driver's interpreter step (the documented per-instruction deopt).
        if self.compiled.entry_pc(func)? != pc {
            return None;
        }
        // An inlined `move` addresses `machine.x` unchecked, so a machine whose
        // register file is shorter than the highest natively addressed index cannot
        // enter native code; such a run stays on the interpreter step.
        if let Some(highest) = self.compiled.highest_native_register {
            // SAFETY: `rt` is the live driver-owned runtime block and its `machine`
            // pointer points at the driver's live `Machine`; the driver holds no
            // `&mut` alias while the native call is in flight.
            let x_len = unsafe { (*(*rt).machine).x.len() };
            if x_len <= highest as usize {
                return None;
            }
        }
        let entry = self.compiled.entry(func)?;
        // SAFETY: `entry` is the finalized entry of a function declared with the
        // `NativeFunc` signature (`extern "C" fn(i64) -> i64`) in the JIT module that
        // owns it, so the pointer is callable with exactly that ABI; `rt` is the live
        // driver-owned runtime block, passed as the `i64` first parameter (a pointer
        // and an `i64` use the same argument register under the System V AMD64 ABI).
        let code: NativeFunc = unsafe { mem::transmute(entry) };
        Some(code(rt as i64))
    }
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// Turn a Cranelift module error into the engine's documented typed error.
///
/// Cranelift failures (an unsupported host ISA, a declaration/signature clash, a
/// codegen or allocation failure) are not a BEAM exception: they are reported with
/// the engine's `unsupported` sentinel, carrying Cranelift's own message, rather
/// than invented.
fn module_error(err: ModuleError) -> beam_common::BeamError {
    error::unsupported(&format!("cranelift jit: {err}"))
}

/// Compile every IR function of `module` into one Cranelift function.
///
/// Functions without code (or without a resolvable entry label) are skipped: the
/// driver simply keeps interpreting them, which is always correct.
fn compile_module(
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

    // One shared signature per IR function: `(rt: i64) -> i64`.
    let mut function_sig = jit.make_signature();
    function_sig.params.push(AbiParam::new(types::I64));
    function_sig.returns.push(AbiParam::new(types::I64));

    // The module-wide `Label -> (func, pc)` map an inlined `jump` resolves its
    // target through.
    let labels = crate::interpreter::build_label_map(module);

    let mut entry_pc: HashMap<u32, u32> = HashMap::new();
    let mut defined: Vec<(u32, FuncId)> = Vec::new();
    let mut highest_native_register: Option<u32> = None;
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
            &mut highest_native_register,
        )?;        entry_pc.insert(func_index, pc as u32);
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
        highest_native_register,
    })
}

/// Emit one cranelift function for IR function `function` (`func_index` of
/// `module`), starting at `entry_pc`; see the module docs for the pc-block CFG.
#[allow(clippy::too_many_arguments)]
fn emit_function(
    jit: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    helper_id: FuncId,
    signature: &Signature,
    id: FuncId,
    module: &Module,
    func_index: u32,
    labels: &BTreeMap<Label, (u32, u32)>,    function: &Function,
    entry_pc: u32,
    highest_native_register: &mut Option<u32>,
) -> Result<()> {
    let code = &function.code;
    let code_len = code.len();
    let candidates = candidate_pcs(function);
    let mut ctx = jit.make_context();
    ctx.func.signature = signature.clone();
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, builder_context);
        let helper = jit.declare_func_in_func(helper_id, &mut builder.func);

        // --- entry: jump into the entry `pc`'s block. --------------------------
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.append_block_params_for_function_params(entry);
        let rt = builder.block_params(entry)[0];
        let this_func = builder.ins().iconst(types::I32, i64::from(func_index));

        let mut cfg = Cfg {
            pc_blocks: HashMap::new(),
            pending: Vec::new(),
            emitted: HashSet::new(),
            leave: None,
            dispatch: None,
            dispatch_emitted: false,
        };
        let start = block_for(&mut builder, &mut cfg, entry_pc);
        builder.ins().jump(start, &[]);

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
                    highest_native_register,
                );            }
            // The shared dispatch chain is filled once per function, after every
            // `pc` block reached so far (see `emit_dispatch_chain`).
            match cfg.dispatch {
                Some(dispatch) if !cfg.dispatch_emitted => {
                    cfg.dispatch_emitted = true;
                    builder.switch_to_block(dispatch);
                    emit_dispatch_chain(&mut builder, &mut cfg, code_len, &candidates);
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
struct Cfg {
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
fn goto_or_leave(
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
    labels: &BTreeMap<Label, (u32, u32)>,
    func_index: u32,
    helper: FuncRef,
    rt: Value,
    this_func: Value,
    highest_native_register: &mut Option<u32>,
) {
    match code.get(pc as usize) {        // Metadata no-ops (`ops.tab:103-108`): they never change control flow, so the
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
                emit_inline_move(builder, rt, dst_x, &source, highest_native_register);
                goto_or_leave(builder, cfg, code_len, func_index, pc + 1);
                return;
            }
        }
        Some(Instr::Jump { target }) => {
            if let Some(&(target_func, target_pc)) = labels.get(target) {
                if target_func == func_index && (target_pc as usize) < code_len {
                    let block = block_for(builder, cfg, target_pc);
                    builder.ins().jump(block, &[]);
                } else {
                    // A jump into another IR function (or to an address this function
                    // has no code for) leaves this native function: hand the encoded
                    // `(func, pc)` word to the driver.
                    let word = builder
                        .ins()
                        .iconst(types::I64, encode_target(target_func, target_pc));
                    builder.ins().return_(&[word]);
                }
                return;
            }
            // An undefined label keeps the interpreter's exact `unknown_label` error:
            // hand the jump to the helper rather than inventing one.
        }
        // The one BIF the tier inlines: the small-int `erlang:'+'/2`/`'-'/2` fast
        // path. It is a *shape* test (never a value test) — every mismatch, and every
        // run-time guard failure, falls through to the helper, whose interpreter step
        // is the BIF's exact semantics (including the `fail`-label / raise split).
        Some(gc_bif @ Instr::GcBif { .. }) => {
            if let Some(fast) = fast_arith(module, gc_bif) {
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
                    highest_native_register,
                );                return;
            }
        }
        _ => {}
    }    // Everything else: one interpreter step, then dispatch on the word it returned.
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
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
fn emit_helper_dispatch(
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
// ---------------------------------------------------------------------------
// The inlined instruction set
// ---------------------------------------------------------------------------

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

/// Where an inlined `move`'s value comes from.
enum MoveSource {
    /// An x-register (`machine.x[n]`).
    X(u32),
    /// A constant operand's raw `Eterm` word.
    Constant(i64),
}

/// The `(destination x-register, source)` of a `move` the tier can inline, or
/// `None` when the block must deopt to the helper.
///
/// Only x-register destinations and x-register / constant sources are inlinable; a
/// `Y` (or other) operand, or an x-register index past the engine's
/// [`MAX_REGISTER`] bound, deopts so the interpreter's exact `bad_register` error
/// surfaces.
fn inline_move(src: &Operand, dst: &Operand) -> Option<(u32, MoveSource)> {
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER {
        return None;
    }
    let source = match x_register(src) {
        Some(src_x) if src_x < MAX_REGISTER => MoveSource::X(src_x),
        Some(_) => return None,
        None => MoveSource::Constant(constant_word(src)?),
    };
    Some((dst_x, source))
}

/// Emit an inlined `move`: read the source (an `machine.x` slot or a constant raw
/// word) and write the destination x-register slot, both in `machine.x`.
fn emit_inline_move(
    builder: &mut FunctionBuilder,
    rt: Value,
    dst_x: u32,
    source: &MoveSource,
    highest_native_register: &mut Option<u32>,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    let value = match source {
        MoveSource::X(src_x) => {
            touch(highest_native_register, *src_x);
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), x_regs, x_offset(*src_x))
        }
        MoveSource::Constant(word) => builder.ins().iconst(types::I64, *word),
    };
    builder
        .ins()
        .store(MemFlags::trusted(), value, x_regs, x_offset(dst_x));
    touch(highest_native_register, dst_x);
}

// ---------------------------------------------------------------------------
// The inlined small-int arithmetic fast path
// ---------------------------------------------------------------------------

/// The two BIFs the inlined fast path lowers — `erlang:'+'/2` and `erlang:'-'/2`,
/// and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SmallArith {
    /// `erlang:'+'/2`.
    Add,
    /// `erlang:'-'/2`.
    Sub,
}

/// `beam_term::TAG_IMMED1_MASK` as an `i64` — the low-nibble mask `is_small` tests
/// with (`(word & 0xF) == 0xF`).
const SMALL_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `beam_term::TAG_IMMED1_SMALL` as an `i64` — the immediate-1 small tag, which is
/// also the low nibble of [`SMALL_MASK`] (so one mask serves as the guard's tag).
const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The shift `make_small` applies (`beam_term::TAG_IMMED1_SIZE`).
const SMALL_SHIFT: i64 = TAG_IMMED1_SIZE as i64;/// Where an inlined `gc_bif` reads one operand from.
#[derive(Clone, Copy)]
enum FastOperand {
    /// An x-register (`machine.x[n]`).
    X(u32),
    /// A small-int constant's raw `Eterm` word.
    Constant(i64),
}

/// A `gc_bif` shape the fast path lowers, resolved at compile time.
struct FastArith {
    /// Which of the two BIFs is called.
    op: SmallArith,
    /// The first operand.
    a: FastOperand,
    /// The second operand.
    b: FastOperand,
    /// The x-register index the result is written to.
    dst_x: u32,
}

/// The `gc_bif` shape the fast path lowers, or `None`.
///
/// The shape is the small-int `erlang:'+'/2`/`'-'/2` of arity 2 whose every operand
/// is an x-register (below the engine's [`MAX_REGISTER`] bound) or a **small-int
/// constant**, and whose destination is such an x-register. The BIF is classified by
/// exactly the strings the interpreter's `GcBif` arm hands to
/// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif), so the fast
/// path can never lower a call the interpreter routes somewhere else.
fn fast_arith(module: &Module, instr: &Instr) -> Option<FastArith> {
    let Instr::GcBif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    if *arity != 2 || args.len() != 2 {
        return None;
    }
    let import = module.imports.get(bif.0 as usize)?;
    if module.atom_name(import.module)? != "erlang" || import.arity != 2 {
        return None;
    }
    let op = match module.atom_name(import.function)? {
        "+" => SmallArith::Add,
        "-" => SmallArith::Sub,
        _ => return None,
    };
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER {
        return None;
    }
    Some(FastArith {
        op,
        a: fast_operand(&args[0])?,
        b: fast_operand(&args[1])?,
        dst_x,
    })
}

/// The operand the fast path can load: an x-register, or a constant that **is** a
/// small integer so the run-time guard cannot reject it.
fn fast_operand(operand: &Operand) -> Option<FastOperand> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(FastOperand::X(x)),
        Some(_) => None,
        None => {
            let word = constant_word(operand)?;
            ((word & SMALL_MASK) == SMALL_TAG).then_some(FastOperand::Constant(word))
        }
    }
}

/// Emit the inlined small-int arithmetic fast path for the `gc_bif` at `pc`.
///
/// The fast path is taken only when **both** operands are tagged small integers and
/// the exact result is a representable small integer; every other case — either
/// operand not a small int, or a sum/difference outside
/// `[MIN_SMALL, MAX_SMALL]` — branches to the helper, whose interpreter step is the
/// BIF's exact semantics (including the `fail`-label guard / raise split and the
/// bignum / float / `badarith` paths). So the fast path can only ever *replace* a
/// successful BIF call with the identical tagged word; it never fabricates a result.
///
/// The arithmetic is done on the **untagged** values, which is what makes it exact
/// without an overflow flag: a tagged small word is `v*16 + 15`, so `(word - 15) >> 4`
/// recovers `v` exactly (the shift is arithmetic and the operand is an exact multiple
/// of 16). Two recovered values are each within `±2^59`, so their sum/difference
/// always fits an `i64` — unlike the tagged words, whose sum can overflow — and the
/// range test against `[MIN_SMALL, MAX_SMALL]` is then exactly `make_small`'s domain.
#[allow(clippy::too_many_arguments)]
fn emit_inline_arith(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    fast: &FastArith,
    helper: FuncRef,
    this_func: Value,
    highest_native_register: &mut Option<u32>,
) {    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    let a = load_fast_operand(builder, x_regs, &fast.a, highest_native_register);
    let b = load_fast_operand(builder, x_regs, &fast.b, highest_native_register);

    let tagged_a = builder.ins().band_imm(a, SMALL_MASK);
    let tagged_b = builder.ins().band_imm(b, SMALL_MASK);
    let a_is_small = builder.ins().icmp_imm(IntCC::Equal, tagged_a, SMALL_TAG);
    let b_is_small = builder.ins().icmp_imm(IntCC::Equal, tagged_b, SMALL_TAG);
    let both_small = builder.ins().band(a_is_small, b_is_small);

    let slow = builder.create_block();
    let check = builder.create_block();
    builder.ins().brif(both_small, check, &[], slow, &[]);

    // Both operands are small: recover the untagged values and do the arithmetic.
    builder.switch_to_block(check);
    let untagged_a = builder.ins().iadd_imm(a, -SMALL_TAG);
    let untagged_b = builder.ins().iadd_imm(b, -SMALL_TAG);
    let value_a = builder.ins().sshr_imm(untagged_a, SMALL_SHIFT);
    let value_b = builder.ins().sshr_imm(untagged_b, SMALL_SHIFT);
    let sum = match fast.op {
        SmallArith::Add => builder.ins().iadd(value_a, value_b),
        SmallArith::Sub => builder.ins().isub(value_a, value_b),
    };
    let low_ok = builder
        .ins()
        .icmp_imm(IntCC::SignedGreaterThanOrEqual, sum, MIN_SMALL);
    let high_ok = builder
        .ins()
        .icmp_imm(IntCC::SignedLessThanOrEqual, sum, MAX_SMALL);
    let in_range = builder.ins().band(low_ok, high_ok);

    let store = builder.create_block();
    builder.ins().brif(in_range, store, &[], slow, &[]);

    // Success: store `make_small(sum)` — `sum * 16 + 15` — into the destination slot
    // and continue at `pc + 1`.
    builder.switch_to_block(store);
    let shifted = builder.ins().ishl_imm(sum, SMALL_SHIFT);
    let result = builder.ins().iadd_imm(shifted, SMALL_TAG);
    builder
        .ins()
        .store(MemFlags::trusted(), result, x_regs, x_offset(fast.dst_x));
    touch(highest_native_register, fast.dst_x);
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);

    // Rejected: one interpreter step, the BIF's exact semantics.
    builder.switch_to_block(slow);
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
}
/// Read one fast-path operand: an `machine.x` slot (recording it as natively
/// addressed) or a constant's raw word.
fn load_fast_operand(
    builder: &mut FunctionBuilder,
    x_regs: Value,
    operand: &FastOperand,
    highest_native_register: &mut Option<u32>,
) -> Value {
    match operand {
        FastOperand::X(x) => {
            touch(highest_native_register, *x);
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), x_regs, x_offset(*x))
        }
        FastOperand::Constant(word) => builder.ins().iconst(types::I64, *word),
    }
}
/// The x-register index an operand names, if any (`X(n)` or the `tr`-annotated
/// `Typed { reg: X(n), .. }` — the `Type` annotation is irrelevant to the value).
fn x_register(operand: &Operand) -> Option<u32> {
    match operand {        Operand::X(n) => Some(*n),
        Operand::Typed {
            reg: RegRef::X(n),
            ..
        } => Some(*n),
        _ => None,
    }
}

/// The raw `Eterm` word of a constant operand, as the interpreter's
/// `read_operand` evaluates it (`make_small`/`make_atom`/`NIL`).
///
/// An `Integer`/`Character` outside the small-int range is `None`: the native tier
/// only inlines immediates, so such a constant makes the block **deopt** to
/// `jit_step_one`, whose interpreter step materialises the exact bignum.
fn constant_word(operand: &Operand) -> Option<i64> {
    match operand {
        Operand::Integer(value) => small_word(*value),
        Operand::Character(c) => small_word(i64::from(*c)),
        Operand::Atom(atom) => Some(make_atom(*atom).raw() as i64),
        Operand::Nil => Some(NIL.raw() as i64),
        _ => None,
    }
}

/// The raw `Eterm` word of a small-int constant, or `None` when `value` is outside
/// the 60-bit immediate range (`beam_term::{MIN_SMALL, MAX_SMALL}`).
fn small_word(value: i64) -> Option<i64> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        Some(make_small(value).raw() as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::baseline::HALT;
    use crate::{Machine, MinimalContext};
    use beam_common::BeamError;
    use beam_loader::load_file;
    use beam_term::{is_small, make_small, signed_value};

    /// The `beam_loader` oracle fixture (reused, never copied).
    const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    /// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
    fn arith_hook(
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> core::result::Result<Eterm, BeamError> {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        if !is_small(args[0]) || !is_small(args[1]) {
            return Err(BeamError::BadArith);
        }
        let (a, b) = (signed_value(args[0]), signed_value(args[1]));
        match function {
            "+" => Ok(make_small(a + b)),
            "-" => Ok(make_small(a - b)),
            _ => Err(BeamError::BadArg),
        }
    }

    #[test]
    fn the_tier_names_itself_and_reports_available() {
        let backend = CraneliftJit::new();
        assert_eq!(JitBackend::name(&backend), "jit-cranelift");
        assert!(JitBackend::available(&backend));
    }

    #[test]
    fn compile_emits_one_native_entry_per_non_empty_function() {
        let module = load_file(SLICE_BEAM).expect("load slice.beam");
        let mut backend = CraneliftJit::new();
        let compiled = backend.compile(&module).expect("compile slice.beam");

        let expected: usize = module
            .functions
            .iter()
            .filter(|function| {
                !function.code.is_empty() && function.labels.contains_key(&function.entry_label)
            })
            .count();
        assert!(expected > 0, "the fixture must have functions");
        assert_eq!(compiled.function_count(), expected);

        // Every compiled function's entry is non-null and its entry `pc` is the
        // function's own entry label.
        for (index, function) in module.functions.iter().enumerate() {
            let Some(&pc) = function.labels.get(&function.entry_label) else {
                continue;
            };
            let func = index as u32;
            assert_ne!(compiled.entry(func), Some(std::ptr::null()));
            assert_eq!(compiled.entry_pc(func), Some(pc as u32));
        }
    }

    #[test]
    fn a_function_without_an_entry_label_is_not_compiled() {
        // An empty module compiles to an empty artifact (no functions, no entries) —
        // the driver keeps interpreting it.
        let module = Module::default();
        let mut backend = CraneliftJit::new();
        let compiled = backend.compile(&module).expect("compile an empty module");
        assert_eq!(compiled.function_count(), 0);
        assert_eq!(compiled.highest_native_register(), None);
    }

    #[test]
    fn the_compiled_function_runs_the_export_natively() {
        // Evidence that native code really is entered and executed (rather than the
        // driver silently interpreting): build the runtime block by hand, call the
        // compiled `slice:add/2` entry **directly** and check both the control word
        // its native loop returns and the x-register it left behind.
        let module = load_file(SLICE_BEAM).expect("load slice.beam");
        let add = module
            .atom_table
            .lookup("add")
            .expect("the fixture interns `add`");
        let entry_label = module.export(add, 2).expect("add/2 is exported");
        let labels = crate::interpreter::build_label_map(&module);
        let (func, pc) = crate::interpreter::resolve_label(&module, entry_label)
            .expect("the export label resolves");

        let mut backend = CraneliftJit::new();
        let compiled = backend.compile(&module).expect("compile slice.beam");
        // The function is entered at its own entry address; any other address is the
        // driver's interpreter step.
        assert_eq!(compiled.entry_pc(func), Some(pc));
        let code = CompiledEntries { compiled: &compiled };

        let mut machine = Machine::new();
        machine.reset();
        machine.set_x(0, make_small(2)).expect("seed x0");
        machine.set_x(1, make_small(3)).expect("seed x1");
        machine.push_frame(0).expect("push the bottom frame");
        machine.set_literals(module.literals.clone());

        let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
        let x_regs: *mut Eterm = machine.x.as_mut_ptr();
        let machine_ptr: *mut Machine = &mut machine;
        let ctx_ptr = &mut ctx as *mut dyn ExecutionContext;
        let mut rt = JitRuntime {
            machine: machine_ptr,
            x_regs,
            module: &module as *const Module,
            labels: &labels as *const _,
            ctx: ctx_ptr,
            error: None,
        };

        let word = code
            .enter(&mut rt, func, pc)
            .expect("the entry address is compiled");
        // The native CFG ran to the function's `return`: the bottom frame's
        // continuation, i.e. HALT.
        assert_eq!(word, HALT);
        // SAFETY: `machine_ptr` points at the live `machine` above; no native code is
        // running here.
        assert_eq!(unsafe { (*machine_ptr).x(0) }, Some(make_small(5)));
    }

    #[test]
    fn inlined_moves_are_reported_as_touched_registers() {
        // `slice.beam`'s `add/2` body moves x-registers around, so the artifact must
        // record the highest one the inlined fast path addresses in `machine.x`.
        let module = load_file(SLICE_BEAM).expect("load slice.beam");
        let compiled = CraneliftJit::new()
            .compile(&module)
            .expect("compile slice.beam");
        assert!(
            compiled.highest_native_register().is_some(),
            "the fixture must inline at least one move"
        );
    }
}
